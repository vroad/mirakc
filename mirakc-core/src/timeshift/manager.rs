use std::str::FromStr;
use std::sync::Arc;

use actlet::prelude::*;
use chrono_jst::Jst;
use indexmap::IndexMap;
use itertools::Itertools;

use super::recorder::TimeshiftRecorder;
use super::*;
use crate::config::Config;
use crate::epg;
use crate::error::Error;
use crate::models::*;
use crate::tuner::StartStreaming;
use crate::tuner::StopStreaming;

pub struct TimeshiftManager<T, E> {
    config: Arc<Config>,
    tuner_manager: T,
    epg: E,
    recorders: IndexMap<String, RecorderHolder<T>>,
    event_emitters: EmitterRegistry<TimeshiftEvent>,
}

impl<T, E> TimeshiftManager<T, E> {
    pub fn new(config: Arc<Config>, tuner_manager: T, epg: E) -> Self {
        TimeshiftManager {
            config,
            tuner_manager,
            epg,
            recorders: IndexMap::new(),
            event_emitters: Default::default(),
        }
    }
}

// actor

#[async_trait]
impl<T, E> Actor for TimeshiftManager<T, E>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
    E: Send + Sync + 'static,
    E: Call<epg::RegisterEmitter>,
{
    async fn started(&mut self, ctx: &mut Context<Self>) {
        tracing::debug!("Started");

        // Recorders will be activated in the message handler.
        self.epg
            .call(epg::RegisterEmitter::ServicesUpdated(ctx.emitter()))
            .await
            .expect("Failed to register the emitter");

        // Create recorders regardless of whether its service is available or not.
        for (index, name) in self.config.timeshift.recorders.keys().enumerate() {
            let addr = ctx
                .spawn_actor(TimeshiftRecorder::new(
                    index,
                    name.clone(),
                    self.config.clone(),
                    self.tuner_manager.clone(),
                    ctx.emitter(),
                ))
                .await;
            let holder = RecorderHolder::new(addr);
            self.recorders.insert(name.clone(), holder);
        }

        // Perform health check for each recorder at 50s every minute.
        let task = {
            let addr = ctx.address().clone();
            async move {
                let schedule = cron::Schedule::from_str("50 * * * * * *").unwrap();
                for next in schedule.upcoming(Jst) {
                    let interval = (next - Jst::now()).to_std().unwrap();
                    tokio::time::sleep(interval).await;
                    if let Err(_) = addr.call(HealthCheck).await {
                        // The manager has been gone.
                        return;
                    }
                }
            }
        };
        ctx.spawn_task(task);
    }

    async fn stopped(&mut self, _ctx: &mut Context<Self>) {
        for (name, recorder) in self.recorders.iter() {
            tracing::debug!(recorder.name = name, "Waitting for the recorder to stop...");
            recorder.addr.wait().await;
        }
        tracing::debug!("Stopped");
    }
}

// register emitter

#[async_trait]
impl<T, E> Handler<RegisterEmitter> for TimeshiftManager<T, E>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
    E: Send + Sync + 'static,
    E: Call<epg::RegisterEmitter>,
{
    async fn handle(
        &mut self,
        msg: RegisterEmitter,
        ctx: &mut Context<Self>,
    ) -> <RegisterEmitter as Message>::Reply {
        // Create a task to send messages.
        //
        // Sending many messages in the message handler may cause a dead lock
        // when the number of messages to be sent is larger than the capacity
        // of the emitter's channel.  See the issue #705 for example.
        let task = {
            let recorder_states = self
                .recorders
                .iter()
                .map(|(name, recorder)| {
                    (
                        name.clone(),
                        recorder.started,
                        recorder.current_record_id.clone(),
                    )
                })
                .collect_vec();
            let emitter = msg.0.clone();
            async move {
                for (recorder, started, current_record_id) in recorder_states.into_iter() {
                    if started {
                        let recorder = recorder.clone();
                        let msg = TimeshiftEvent::Started { recorder };
                        emitter.emit(msg).await;
                    }
                    if let Some(record_id) = current_record_id {
                        let recorder = recorder.clone();
                        let msg = TimeshiftEvent::RecordStarted {
                            recorder,
                            record_id,
                        };
                        emitter.emit(msg).await;
                    }
                }
            }
        };
        ctx.spawn_task(task);

        let id = self.event_emitters.register(msg.0);
        tracing::debug!(msg.name = "RegisterEmitter", id);
        id
    }
}

// unregister emitter

#[async_trait]
impl<T, E> Handler<UnregisterEmitter> for TimeshiftManager<T, E>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
    E: Send + Sync + 'static,
    E: Call<epg::RegisterEmitter>,
{
    async fn handle(&mut self, msg: UnregisterEmitter, _ctx: &mut Context<Self>) {
        tracing::debug!(msg.name = "UnregisterEmitter", id = msg.0);
        self.event_emitters.unregister(msg.0);
    }
}

// query timeshift recorders

#[async_trait]
impl<T, E> Handler<QueryTimeshiftRecorders> for TimeshiftManager<T, E>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
    E: Send + Sync + 'static,
    E: Call<epg::RegisterEmitter>,
{
    async fn handle(
        &mut self,
        _msg: QueryTimeshiftRecorders,
        _ctx: &mut Context<Self>,
    ) -> <QueryTimeshiftRecorders as Message>::Reply {
        let mut models = vec![];
        for (index, recorder) in self.recorders.values().enumerate() {
            models.push(
                recorder
                    .addr
                    .call(QueryTimeshiftRecorder {
                        recorder: TimeshiftRecorderQuery::ByIndex(index),
                    })
                    .await??,
            );
        }
        Ok(models)
    }
}

// forward messages to a specified recorder

macro_rules! impl_proxy_handler {
    ($msg:ty) => {
        #[async_trait]
        impl<T, E> Handler<$msg> for TimeshiftManager<T, E>
        where
            T: Clone + Send + Sync + 'static,
            T: Call<StartStreaming>,
            T: TriggerFactory<StopStreaming>,
            E: Send + Sync + 'static,
            E: Call<epg::RegisterEmitter>,
        {
            async fn handle(
                &mut self,
                msg: $msg,
                _ctx: &mut Context<Self>,
            ) -> <$msg as Message>::Reply {
                let maybe_recorder = match msg.recorder {
                    TimeshiftRecorderQuery::ByIndex(index) => self
                        .recorders
                        .get_index(index)
                        .map(|(_, recorder)| recorder.addr.clone())
                        .ok_or(Error::RecordNotFound),
                    TimeshiftRecorderQuery::ByName(ref name) => self
                        .recorders
                        .get(name)
                        .map(|recorder| recorder.addr.clone())
                        .ok_or(Error::RecordNotFound),
                };
                maybe_recorder?.call(msg).await?
            }
        }
    };
}

impl_proxy_handler!(QueryTimeshiftRecorder);
impl_proxy_handler!(QueryTimeshiftRecords);
impl_proxy_handler!(QueryTimeshiftRecord);
impl_proxy_handler!(CreateTimeshiftLiveStreamSource);
impl_proxy_handler!(CreateTimeshiftRecordStreamSource);

// health check

#[async_trait]
impl<T, E> Handler<HealthCheck> for TimeshiftManager<T, E>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
    E: Send + Sync + 'static,
    E: Call<epg::RegisterEmitter>,
{
    async fn handle(
        &mut self,
        _msg: HealthCheck,
        ctx: &mut Context<Self>,
    ) -> <HealthCheck as Message>::Reply {
        tracing::debug!(msg.name = "HealthCheck");
        for (index, (name, recorder)) in self.recorders.iter_mut().enumerate() {
            if let Err(_) = recorder.addr.call(HealthCheck).await {
                // The recorder has been gone.  Respawn it.
                assert!(!recorder.addr.is_available());
                let addr = ctx
                    .spawn_actor(TimeshiftRecorder::new(
                        index,
                        name.clone(),
                        self.config.clone(),
                        self.tuner_manager.clone(),
                        ctx.emitter(),
                    ))
                    .await;
                recorder.addr = addr;
                recorder.started = false;
                recorder.current_record_id = None;
            }
        }
    }
}

// services updated

#[async_trait]
impl<T, E> Handler<epg::ServicesUpdated> for TimeshiftManager<T, E>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
    E: Send + Sync + 'static,
    E: Call<epg::RegisterEmitter>,
{
    async fn handle(&mut self, msg: epg::ServicesUpdated, _ctx: &mut Context<Self>) {
        tracing::debug!(msg.name = "ServicesUpdated");
        for (name, holder) in self.recorders.iter() {
            let config = self.config.timeshift.recorders.get(name).unwrap();
            let msg = ServiceUpdated {
                service: msg.services.get(&config.service_id).cloned(),
            };
            holder.addr.emit(msg).await;
        }
    }
}

// timeshift event

#[async_trait]
impl<T, E> Handler<TimeshiftEvent> for TimeshiftManager<T, E>
where
    T: Clone + Send + Sync + 'static,
    T: Call<StartStreaming>,
    T: TriggerFactory<StopStreaming>,
    E: Send + Sync + 'static,
    E: Call<epg::RegisterEmitter>,
{
    async fn handle(&mut self, msg: TimeshiftEvent, _ctx: &mut Context<Self>) {
        tracing::debug!(msg.name = "TimeshiftEvent");
        // Update the recorder states.
        // See the comment in `RecorderHolder`.
        match msg {
            TimeshiftEvent::Started { ref recorder } => {
                if let Some(recorder) = self.recorders.get_mut(recorder) {
                    recorder.started = true;
                }
            }
            TimeshiftEvent::Stopped { ref recorder } => {
                if let Some(recorder) = self.recorders.get_mut(recorder) {
                    recorder.started = false;
                }
            }
            TimeshiftEvent::RecordStarted {
                ref recorder,
                record_id,
            } => {
                if let Some(recorder) = self.recorders.get_mut(recorder) {
                    recorder.current_record_id = Some(record_id);
                }
            }
            TimeshiftEvent::RecordEnded { ref recorder, .. } => {
                if let Some(recorder) = self.recorders.get_mut(recorder) {
                    recorder.current_record_id = None;
                }
            }
            _ => (),
        }
        self.event_emitters.emit(msg).await;
    }
}

// models

struct RecorderHolder<T> {
    addr: Address<TimeshiftRecorder<T>>,

    // Cache the following recorder states in order to emit preceding events
    // when a new emitter is registered.
    //
    // We can fetch these states by using `QueryTimeshiftRecorder` in the
    // `RegisterEmitter` handler, but this may break consistency of the event
    // sequence.  Because a `TimeshiftEvent` message could be sent to the
    // manager while it's handling the RegisterEmitter message.
    started: bool,
    current_record_id: Option<TimeshiftRecordId>,
}

impl<T> RecorderHolder<T> {
    fn new(addr: Address<TimeshiftRecorder<T>>) -> Self {
        RecorderHolder {
            addr,
            started: false,
            current_record_id: None,
        }
    }
}
