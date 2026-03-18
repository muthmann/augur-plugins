use augur_plugin_api::{
    export_plugin, EventStoreHandle, FfiPixel, HostContext, HostOutput, Plugin, PluginFrame,
    SettingItem, SettingKind, SettingsSchema, SettingsSection, StatusEntry,
};
use serde_json::{json, Value};

#[derive(Default)]
pub struct TemplatePlugin {
    enabled: bool,
    threshold: u16,
    last_hits: usize,
}

impl Plugin for TemplatePlugin {
    fn name(&self) -> &'static str {
        "Template Plugin"
    }

    fn description(&self) -> &'static str {
        "A starting point for a runtime-loaded AugurRS plugin."
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    fn reset(&mut self) {
        self.last_hits = 0;
    }

    fn process_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        output: &mut HostOutput<'_>,
        _context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        let mut pixels = Vec::new();
        for (index, value) in frame.pixels().iter().enumerate() {
            if *value < self.threshold {
                continue;
            }
            let x = (index % frame.width() as usize) as u16;
            let y = (index / frame.width() as usize) as u16;
            pixels.push(FfiPixel { x, y });
            if pixels.len() >= 256 {
                break;
            }
        }

        self.last_hits = pixels.len();
        if !pixels.is_empty() {
            output.add_highlight_pixels(&pixels, [255, 180, 0, 160]);
        }
    }

    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema {
            sections: vec![SettingsSection {
                label: "Detection".into(),
                description: Some(
                    "Example declarative setting exposed through the host UI.".into(),
                ),
                default_open: true,
                items: vec![SettingItem {
                    key: "threshold".into(),
                    label: "Pixel threshold".into(),
                    tooltip: Some("Pixels at or above this preview count are highlighted.".into()),
                    kind: SettingKind::I64Slider {
                        min: 0,
                        max: 4_096,
                        default: i64::from(self.threshold),
                        suffix: None,
                    },
                }],
            }],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            "threshold" => Some(json!(self.threshold)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "threshold" => {
                let Some(value) = value.as_u64() else {
                    return Err("threshold must be an integer".into());
                };
                self.threshold =
                    u16::try_from(value.clamp(0, 4_096)).expect("clamped threshold fits in u16");
                Ok(())
            }
            _ => Err(format!("unknown setting: {key}")),
        }
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        vec![StatusEntry::Text(format!(
            "Last frame: {} highlighted pixels.",
            self.last_hits
        ))]
    }
}

export_plugin!(TemplatePlugin);
