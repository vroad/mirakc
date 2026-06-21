use super::*;

use crate::filter::FilterPipelineBuilder;
use crate::models::TunerUser;
use crate::timeshift;
use crate::timeshift::TimeshiftRecorderModel;
use crate::timeshift::TimeshiftRecorderQuery;
use crate::tuner;
use crate::web::api::stream::StreamingHeaderParams;
use crate::web::api::stream::do_head_stream;
use crate::web::api::stream::streaming;

/// Gets a media stream of the tuner feeding a timeshift recorder.
///
/// Unlike `/timeshift/{recorder}/stream`, which serves recorded content from
/// the ring buffer, this endpoint taps the TS packets that the tuner command is
/// currently producing for the recorder.  It subscribes to the same tuner
/// session the recorder is using and therefore never allocates or grabs a
/// tuner.  When the recorder is not currently recording, no tuner output is
/// available to observe and the request fails.
#[utoipa::path(
    get,
    path = "/timeshift/{recorder}/tuner-stream",
    params(
        ("X-Mirakurun-Priority" = Option<i32>, Header, description = "Priority of the tuner user"),
        ("recorder" = String, Path, description = "Timeshift recorder name"),
        FilterSetting,
    ),
    responses(
        (status = 200, description = "OK",
         headers(
             ("X-Mirakurun-Tuner-User-ID" = String, description = "Tuner user ID"),
         ),
        ),
        (status = 404, description = "Not Found"),
        (status = 500, description = "Internal Server Error"),
        (status = 503, description = "Tuner Resource Unavailable"),
    ),
    operation_id = "getTimeshiftRecorderTunerStream",
)]
pub(in crate::web::api) async fn get<T, S, W>(
    State(ConfigExtractor(config)): State<ConfigExtractor>,
    State(TunerManagerExtractor(tuner_manager)): State<TunerManagerExtractor<T>>,
    State(TimeshiftManagerExtractor(timeshift_manager)): State<TimeshiftManagerExtractor<S>>,
    State(SpawnerExtractor(spawner)): State<SpawnerExtractor<W>>,
    Path(recorder_id): Path<String>,
    user: TunerUser,
    Qs(filter_setting): Qs<FilterSetting>,
) -> Result<Response, Error>
where
    T: Clone,
    T: Call<tuner::StartStreaming>,
    T: TriggerFactory<tuner::StopStreaming>,
    S: Call<timeshift::QueryTimeshiftRecorder>,
    W: Spawn,
{
    let msg = timeshift::QueryTimeshiftRecorder {
        recorder: TimeshiftRecorderQuery::ByName(recorder_id),
    };
    let recorder = timeshift_manager.call(msg).await??;

    // Tap the tuner session that currently feeds the recorder.  If the recorder
    // is not recording, there is no session to observe and we must NOT allocate
    // a tuner.  Passing the recorder's subscription ID makes the tuner manager
    // take the "reuse specified tuner" path, which fails instead of grabbing a
    // tuner when the session is gone.
    let stream_id = recorder
        .tuner_subscription_id
        .ok_or(Error::TunerUnavailable)?;

    let stream = tuner_manager
        .call(tuner::StartStreaming {
            channel: recorder.service.channel.clone(),
            user: user.clone(),
            stream_id: Some(stream_id),
        })
        .await??;

    // stop_trigger must be created here in order to stop streaming when an
    // error occurs.
    let msg = tuner::StopStreaming { id: stream.id() };
    let stop_trigger = tuner_manager.trigger(msg);

    let (filters, content_type, seekable) =
        build_filters(&config, &user, &filter_setting, &recorder, stream.is_decoded())?;
    debug_assert!(!seekable);

    // Ignore the range header.

    let params = StreamingHeaderParams {
        seekable,
        content_type,
        length: None,
        range: None,
        user,
    };

    streaming(&config, &spawner, stream, filters, &params, stop_trigger).await
}

#[utoipa::path(
    head,
    path = "/timeshift/{recorder}/tuner-stream",
    params(
        ("X-Mirakurun-Priority" = Option<i32>, Header, description = "Priority of the tuner user"),
        ("recorder" = String, Path, description = "Timeshift recorder name"),
        FilterSetting,
    ),
    responses(
        (status = 200, description = "OK",
         headers(
             ("X-Mirakurun-Tuner-User-ID" = String, description = "Tuner user ID"),
         ),
        ),
        (status = 404, description = "Not Found"),
        (status = 500, description = "Internal Server Error"),
        (status = 503, description = "Tuner Resource Unavailable"),
    ),
    operation_id = "checkTimeshiftRecorderTunerStream",
)]
pub(in crate::web::api) async fn head<S>(
    State(ConfigExtractor(config)): State<ConfigExtractor>,
    State(TimeshiftManagerExtractor(timeshift_manager)): State<TimeshiftManagerExtractor<S>>,
    Path(recorder_id): Path<String>,
    user: TunerUser,
    Qs(filter_setting): Qs<FilterSetting>,
) -> Result<Response, Error>
where
    S: Call<timeshift::QueryTimeshiftRecorder>,
{
    let msg = timeshift::QueryTimeshiftRecorder {
        recorder: TimeshiftRecorderQuery::ByName(recorder_id),
    };
    let recorder = timeshift_manager.call(msg).await??;

    let (_, content_type, seekable) = build_filters(
        &config,
        &user,
        &filter_setting,
        &recorder,
        false, // This is a dummy but works properly.
    )?;
    debug_assert!(!seekable);

    let params = StreamingHeaderParams {
        seekable,
        content_type,
        length: None,
        range: None,
        user,
    };

    // This endpoint returns a positive response even when the recorder is not
    // recording at this point.  No one knows whether this request handler will
    // succeed or not until actually starting streaming.
    do_head_stream(&params)
}

fn build_filters(
    config: &Config,
    user: &TunerUser,
    filter_setting: &FilterSetting,
    recorder: &TimeshiftRecorderModel,
    decoded: bool,
) -> Result<(Vec<String>, String, bool), Error> {
    let channel = &recorder.service.channel;
    let data = mustache::MapBuilder::new()
        .insert_str("channel_name", &channel.name)
        .insert("channel_type", &channel.channel_type)?
        .insert_str("channel", &channel.channel)
        .insert("sid", &recorder.service.id.sid())?
        .insert("user", &user)?
        .build();

    let mut builder = FilterPipelineBuilder::new(data, false); // not seekable
    builder.add_pre_filters(&config.pre_filters, &filter_setting.pre_filters)?;
    if !decoded && filter_setting.decode {
        builder.add_decode_filter(&config.filters.decode_filter)?;
    }
    builder.add_post_filters(&config.post_filters, &filter_setting.post_filters)?;
    Ok(builder.build())
}
