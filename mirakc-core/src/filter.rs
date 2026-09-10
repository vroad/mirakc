use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::config::FilterConfig;
use crate::config::PostFilterConfig;
use crate::config::PreFilterConfig;
use crate::error::Error;

pub struct FilterPipelineBuilder {
    data: mustache::Data,
    filter_vars: Option<mustache::Data>,
    filters: Vec<String>,
    content_type: String,
    seekable: bool,
}

impl FilterPipelineBuilder {
    pub fn new(
        data: mustache::MapBuilder,
        seekable: bool,
        filter_vars: Option<&BTreeMap<String, String>>,
    ) -> Self {
        let filter_vars = filter_vars.filter(|vars| !vars.is_empty()).map(|vars| {
            vars.iter()
                .fold(mustache::MapBuilder::new(), |builder, (key, value)| {
                    builder.insert_str(key, value)
                })
                .build()
        });
        FilterPipelineBuilder {
            data: data.build(),
            filter_vars,
            filters: Vec::new(),
            content_type: "video/MP2T".to_string(),
            seekable,
        }
    }

    pub fn build(self) -> (Vec<String>, String, bool) {
        (self.filters, self.content_type, self.seekable)
    }

    pub fn add_pre_filters(
        &mut self,
        pre_filters: &HashMap<String, PreFilterConfig>,
        names: &[String],
    ) -> Result<usize, Error> {
        for name in names.iter() {
            if pre_filters.contains_key(name) {
                self.add_pre_filter(&pre_filters[name], name)?;
            } else {
                tracing::warn!(pre_filter = name, "No such pre-filter");
            }
        }
        Ok(self.filters.len())
    }

    pub fn add_service_filter(&mut self, config: &FilterConfig) -> Result<usize, Error> {
        self.add_builtin_filter(config, "service-filter")
    }

    pub fn add_decode_filter(&mut self, config: &FilterConfig) -> Result<usize, Error> {
        self.add_builtin_filter(config, "decode-filter")
    }

    pub fn add_program_filter(&mut self, config: &FilterConfig) -> Result<usize, Error> {
        self.add_builtin_filter(config, "program-filter")
    }

    pub fn add_post_filters(
        &mut self,
        post_filters: &HashMap<String, PostFilterConfig>,
        names: &[String],
    ) -> Result<usize, Error> {
        for name in names.iter() {
            if post_filters.contains_key(name) {
                self.add_post_filter(&post_filters[name], name)?;
            } else {
                tracing::warn!(post_filter = name, "No such post-filter");
            }
        }
        Ok(self.filters.len())
    }

    fn add_pre_filter(&mut self, config: &PreFilterConfig, name: &str) -> Result<usize, Error> {
        if config.command.is_empty() {
            return Ok(self.filters.len());
        }
        let filter = match self.make_filter(&config.command, config.allow_filter_vars) {
            Ok(filter) => filter,
            Err(err) => {
                tracing::error!(%err, pre_filter = name, "Failed to render pre-filter");
                return Err(err);
            }
        };
        if filter.is_empty() {
            tracing::warn!(pre_filter = name, "Empty pre-filter");
        } else {
            self.filters.push(filter);
            if !config.seekable {
                self.seekable = false;
            }
        }
        Ok(self.filters.len())
    }

    fn add_builtin_filter(&mut self, config: &FilterConfig, name: &str) -> Result<usize, Error> {
        if config.command.is_empty() {
            return Ok(self.filters.len());
        }
        let filter = match self.make_filter(&config.command, config.allow_filter_vars) {
            Ok(filter) => filter,
            Err(err) => {
                tracing::error!(%err, filter = name, "Failed to render filter");
                return Err(err);
            }
        };
        if filter.is_empty() {
            tracing::warn!(filter = name, "Empty filter");
        } else {
            self.filters.push(filter);
            self.seekable = false;
        }
        Ok(self.filters.len())
    }

    fn add_post_filter(&mut self, config: &PostFilterConfig, name: &str) -> Result<usize, Error> {
        if config.command.is_empty() {
            return Ok(self.filters.len());
        }
        let filter = match self.make_filter(&config.command, config.allow_filter_vars) {
            Ok(filter) => filter,
            Err(err) => {
                tracing::error!(%err, post_filter = name, "Failed to render post-filter");
                return Err(err);
            }
        };
        if filter.is_empty() {
            tracing::warn!(post_filter = name, "Empty post-filter");
        } else {
            self.filters.push(filter);
            if let Some(content_type) = config.content_type.as_ref() {
                self.content_type.clone_from(content_type);
            }
            if !config.seekable {
                self.seekable = false;
            }
        }
        Ok(self.filters.len())
    }

    fn make_filter(&mut self, command: &str, allow_filter_vars: bool) -> Result<String, Error> {
        let template = mustache::compile_str(command)?;
        let inserted = allow_filter_vars && self.filter_vars.is_some();
        if inserted {
            let mustache::Data::Map(data) = &mut self.data else {
                unreachable!();
            };
            data.insert("filter_vars".to_string(), self.filter_vars.take().unwrap());
        }
        let result = template.render_data_to_string(&self.data);
        if inserted {
            let mustache::Data::Map(data) = &mut self.data else {
                unreachable!();
            };
            self.filter_vars = data.remove("filter_vars");
        }
        Ok(result?.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use test_log::test;

    #[test]
    fn test_make_filter() {
        let channel = channel_gr!("test", "channel");
        let user = tuner_user!(0, web; "user-id");

        let data = mustache::MapBuilder::new()
            .insert_str("channel_name", &channel.name)
            .insert("channel_type", &channel.channel_type)
            .unwrap()
            .insert_str("channel", &channel.channel)
            .insert("user", &user)
            .unwrap();

        let filter_vars = BTreeMap::new();
        let mut builder = FilterPipelineBuilder::new(data, false, Some(&filter_vars));

        assert_matches!(&builder.data, mustache::Data::Map(map) => {
            assert!(!map.contains_key("filter_vars"));
        });

        assert_matches!(builder.make_filter("{{channel_name}}", false), Ok(cmd) => {
            assert_eq!(cmd, "test");
        });
        assert_matches!(builder.make_filter("{{channel_type}}", false), Ok(cmd) => {
            assert_eq!(cmd, "GR");
        });
        assert_matches!(builder.make_filter("{{channel}}", false), Ok(cmd) => {
            assert_eq!(cmd, "channel");
        });
        assert_matches!(builder.make_filter("{{#user}}{{priority}}{{/user}}", false), Ok(cmd) => {
            assert_eq!(cmd, "0");
        });
        // rust-mustache seems to support directly accessing properties.
        assert_matches!(builder.make_filter("{{user.priority}}", false), Ok(cmd) => {
            assert_eq!(cmd, "0");
        });
        assert_matches!(builder.make_filter("{{user.info.Web.id}}", false), Ok(cmd) => {
            assert_eq!(cmd, "user-id");
        });
        assert_matches!(builder.make_filter("{{^user.info.Job}}not job{{/user.info.Job}}", false), Ok(cmd) => {
            assert_eq!(cmd, "not job");
        });
    }

    #[test]
    fn test_filter_vars_in_pre_and_post_filters() {
        let data = mustache::MapBuilder::new();
        let filter_vars = BTreeMap::from([("sid".to_string(), "2056".to_string())]);
        let mut builder = FilterPipelineBuilder::new(data, false, Some(&filter_vars));

        let pre_filters = HashMap::from([(
            "test".to_string(),
            PreFilterConfig {
                allow_filter_vars: true,
                command: "pre-filter --sid={{{filter_vars.sid}}}".to_string(),
                seekable: false,
            },
        )]);
        builder
            .add_pre_filters(&pre_filters, &["test".to_string()])
            .unwrap();

        let post_filters = HashMap::from([(
            "test".to_string(),
            PostFilterConfig {
                allow_filter_vars: true,
                command: "post-filter --sid={{{filter_vars.sid}}}".to_string(),
                content_type: None,
                seekable: false,
            },
        )]);
        builder
            .add_post_filters(&post_filters, &["test".to_string()])
            .unwrap();

        let (filters, _, _) = builder.build();
        assert_eq!(filters, ["pre-filter --sid=2056", "post-filter --sid=2056"]);
    }
    const VAR_COMMAND: &str = "cmd {{sid}}/{{{filter_vars.sid}}} {{#filter_vars}}present{{/filter_vars}}{{^filter_vars}}absent{{/filter_vars}}";

    #[test]
    fn test_filter_vars_permission_per_command() {
        let vars = BTreeMap::from([("sid".to_string(), "2056".to_string())]);
        let mut builder = FilterPipelineBuilder::new(
            mustache::MapBuilder::new().insert_str("sid", "100"),
            true,
            Some(&vars),
        );
        for allow_filter_vars in [true, false, true] {
            let config = FilterConfig {
                command: VAR_COMMAND.to_string(),
                allow_filter_vars,
            };
            builder.add_decode_filter(&config).unwrap();
            builder.add_service_filter(&config).unwrap();
            builder.add_program_filter(&config).unwrap();
            builder
                .add_pre_filter(
                    &PreFilterConfig {
                        command: VAR_COMMAND.to_string(),
                        allow_filter_vars,
                        seekable: true,
                    },
                    "pre",
                )
                .unwrap();
            builder
                .add_post_filter(
                    &PostFilterConfig {
                        command: VAR_COMMAND.to_string(),
                        allow_filter_vars,
                        content_type: Some("text/plain".to_string()),
                        seekable: true,
                    },
                    "post",
                )
                .unwrap();
        }
        let (commands, content_type, seekable) = builder.build();
        let expected: Vec<_> = [
            "cmd 100/2056 present",
            "cmd 100/ absent",
            "cmd 100/2056 present",
        ]
        .into_iter()
        .flat_map(|command| [command; 5])
        .collect();
        assert_eq!(commands, expected);
        assert_eq!(content_type, "text/plain");
        assert!(!seekable);
    }

    #[test]
    fn test_filter_vars_presence_and_request_isolation() {
        let empty = BTreeMap::new();
        let empty_value = BTreeMap::from([("sid".to_string(), String::new())]);
        let first = BTreeMap::from([("sid".to_string(), "1".to_string())]);
        let second = BTreeMap::from([("sid".to_string(), "2".to_string())]);
        let mut builders: Vec<_> = [
            None,
            Some(&empty),
            Some(&empty_value),
            Some(&first),
            Some(&second),
        ]
        .into_iter()
        .map(|vars| FilterPipelineBuilder::new(mustache::MapBuilder::new(), true, vars))
        .collect();
        for _ in 0..2 {
            for (builder, expected) in builders.iter_mut().zip([
                "cmd / absent",
                "cmd / absent",
                "cmd / present",
                "cmd /1 present",
                "cmd /2 present",
            ]) {
                assert_eq!(builder.make_filter(VAR_COMMAND, true).unwrap(), expected);
                assert_eq!(
                    builder.make_filter(VAR_COMMAND, false).unwrap(),
                    "cmd / absent"
                );
            }
        }
    }

    #[test]
    fn test_filter_vars_restored_after_errors() {
        let vars = BTreeMap::from([("sid".to_string(), "2056".to_string())]);
        let data =
            mustache::MapBuilder::new().insert_fn("invalid", Box::new(|_| "{{#".to_string()));
        let mut builder = FilterPipelineBuilder::new(data, false, Some(&vars));
        for command in ["{{#", "{{#invalid}}x{{/invalid}}"] {
            assert!(builder.make_filter(command, true).is_err());
            assert_eq!(
                builder.make_filter(VAR_COMMAND, false).unwrap(),
                "cmd / absent"
            );
            assert_eq!(
                builder.make_filter(VAR_COMMAND, true).unwrap(),
                "cmd /2056 present"
            );
        }
    }
}
