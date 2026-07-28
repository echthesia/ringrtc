//
// Copyright 2024 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

//! Utility functions for audio_device_module.rs
//! Nothing in here should depend on webrtc directly.

use std::{
    borrow::Cow,
    ffi::{CString, c_uchar},
    sync::LazyLock,
};

use anyhow::anyhow;
use cubeb::{DeviceCollection, DeviceState};
#[cfg(target_os = "linux")]
use cubeb_core::DeviceType;
use cubeb_core::{DeviceId, DevicePref};

type StaticRegex =
    regex_automata::dfa::regex::Regex<regex_automata::dfa::sparse::DFA<&'static [u8]>>;

use crate::{webrtc, webrtc::peer_connection_factory::AudioDevice};

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct MinimalDeviceInfo {
    pub devid: DeviceId,
    pub device_id: Option<String>,
    pub friendly_name: String,
    #[cfg(target_os = "linux")]
    device_type: DeviceType,
    preferred: DevicePref,
    state: DeviceState,
}

/// Wrapper struct for DeviceCollection that handles default devices.
///
/// Rather than storing the DeviceCollection directly, which raises complex
/// lifetime issues, store just the fields we need.
///
/// Note that, in some cases, `devid` may be a pointer to state in the cubeb ctx,
/// so in no event should this outlive the associated ctx.
#[derive(PartialEq, Eq, Debug, Clone, Default)]
pub struct DeviceCollectionWrapper {
    device_collection: Vec<MinimalDeviceInfo>,
}

#[cfg(target_os = "linux")]
fn device_is_monitor(device: &MinimalDeviceInfo) -> bool {
    device.device_type == DeviceType::INPUT
        && device
            .device_id
            .as_ref()
            .is_some_and(|s| s.ends_with(".monitor"))
}

impl DeviceCollectionWrapper {
    pub fn new(device_collection: &DeviceCollection<'_>) -> DeviceCollectionWrapper {
        let mut out = Vec::new();
        for device in device_collection.iter() {
            if let Some(friendly) = device.friendly_name() {
                out.push(MinimalDeviceInfo {
                    devid: device.devid(),
                    device_id: device.device_id().as_ref().map(|s| s.to_string()),
                    friendly_name: friendly.to_string(),
                    #[cfg(target_os = "linux")]
                    device_type: device.device_type(),
                    preferred: device.preferred(),
                    state: device.state(),
                })
            } else {
                error!("Device {:?} has no friendly name", device.devid());
            }
        }
        DeviceCollectionWrapper {
            device_collection: out,
        }
    }

    /// Iterate over all Enabled devices (those that are plugged in and not disabled by the OS)
    pub fn iter(
        &self,
    ) -> std::iter::Filter<std::slice::Iter<'_, MinimalDeviceInfo>, fn(&&MinimalDeviceInfo) -> bool>
    {
        self.device_collection
            .iter()
            .filter(|d| d.state == DeviceState::Enabled)
    }

    // For linux only, a method that will ignore "monitor" devices.
    #[cfg(target_os = "linux")]
    pub fn iter_non_monitor(
        &self,
    ) -> std::iter::Filter<std::slice::Iter<'_, MinimalDeviceInfo>, fn(&&MinimalDeviceInfo) -> bool>
    {
        self.device_collection
            .iter()
            .filter(|&d| d.state == DeviceState::Enabled && !device_is_monitor(d))
    }

    #[cfg(target_os = "windows")]
    /// Get a specified device index, accounting for the two default devices.
    pub fn get(&self, idx: usize) -> Option<&MinimalDeviceInfo> {
        // 0 should be "default device" and 1 should be "default communications device".
        // Note: On windows, CUBEB_DEVICE_PREF_VOICE will be set for default communications device,
        // and CUBEB_DEVICE_PREF_MULTIMEDIA | CUBEB_DEVICE_PREF_NOTIFICATION for default device.
        // https://github.com/mozilla/cubeb/blob/bbbe5bb0b29ed64cc7dd191d7a72fe24bba0d284/src/cubeb_wasapi.cpp#L3322
        if self.count() == 0 {
            None
        } else if idx > 1 {
            self.iter().nth(idx - 2)
        } else if idx == 1 {
            // Find a device that's preferred for VOICE -- device 1 is the "default communications"
            self.iter()
                .find(|&device| device.preferred.contains(DevicePref::VOICE))
        } else {
            // Find a device that's preferred for MULTIMEDIA -- device 0 is the "default"
            self.iter()
                .find(|&device| device.preferred.contains(DevicePref::MULTIMEDIA))
        }
    }

    #[cfg(not(target_os = "windows"))]
    /// Get a specified device index, accounting for the default device.
    pub fn get(&self, idx: usize) -> Option<&MinimalDeviceInfo> {
        if self.count() == 0 {
            None
        } else if idx > 0 {
            #[cfg(target_os = "macos")]
            {
                self.iter().nth(idx - 1)
            }
            #[cfg(target_os = "linux")]
            {
                // filter out "monitor" devices.
                self.iter_non_monitor().nth(idx - 1)
            }
        } else {
            // Find a device that's preferred for VOICE -- device 0 is the "default"
            // Even on linux, we do *NOT* filter monitor devices -- if the user specified that as
            // default, we respect it.
            self.iter()
                .find(|&device| device.preferred.contains(DevicePref::VOICE))
        }
    }

    #[cfg(target_os = "windows")]
    /// Returns the number of devices.
    /// Note: On Windows, this is 2 smaller than the number of addressable
    /// devices, because the default device and default communications device
    /// are not counted.
    pub fn count(&self) -> usize {
        self.iter().count()
    }

    #[cfg(not(target_os = "windows"))]
    /// Returns the number of devices, counting the default device.
    pub fn count(&self) -> usize {
        #[cfg(target_os = "macos")]
        let count = self.iter().count();
        #[cfg(target_os = "linux")]
        let count = self.iter_non_monitor().count();
        if count == 0 {
            #[cfg(target_os = "macos")]
            return 0;
            #[cfg(target_os = "linux")]
            return
                // edge case: if there are only monitor devices, and one is the default,
                // allow it.
                if self.iter()
                    .any(|device| device.preferred.contains(DevicePref::VOICE)) {
                        1
                    } else {
                        0
                };
        } else {
            count + 1
        }
    }

    /// Extract all names and IDs, **including repetitions** for the default device(s)!
    pub fn extract_names(&self) -> Vec<Option<AudioDevice>> {
        // On mac and windows, this is relatively simple -- we get the count and then get each reported
        // device.
        #[cfg(not(target_os = "windows"))]
        let count = self.count();

        // On Windows, it's different: count does not include the defaults.
        #[cfg(target_os = "windows")]
        let count = self.count() + 2;

        let mut names = Vec::new();
        for i in 0..count {
            let info = if let Some(info) = self.get(i) {
                info
            } else {
                warn!("Internal error enumerating devices {} vs {}", i, count);
                names.push(None);
                continue;
            };
            let mut name_copy = info.friendly_name.clone();
            #[cfg(not(target_os = "windows"))]
            if i == 0 {
                name_copy = format!("default ({})", info.friendly_name);
            }
            #[cfg(target_os = "windows")]
            {
                if i == 0 {
                    name_copy = format!("Default - {}", info.friendly_name);
                } else if i == 1 {
                    name_copy = format!("Communication - {}", info.friendly_name);
                }
            }
            names.push(Some(AudioDevice {
                // For devices missing unique_id, populate them with name + index
                unique_id: info
                    .device_id
                    .clone()
                    .unwrap_or_else(|| format!("{}-{}", info.friendly_name, i)),
                name: name_copy,
                i18n_key: "".to_string(),
            }));
        }
        names
    }
}

/// Copy from |src| into |dest| at most |dest_size| - 1 bytes and write a nul terminator either after |src| or at the end of |dest_size|
pub fn copy_and_truncate_string(
    src: &str,
    mut dest: webrtc::ptr::Borrowed<c_uchar>,
    dest_size: usize,
) -> anyhow::Result<()> {
    // Leave room for the nul terminator.
    let size = std::cmp::min(src.len(), dest_size - 1);
    let c_str = CString::new(src.get(0..size).ok_or(anyhow!("couldn't get substring"))?)?;
    let c_str_bytes = c_str.as_bytes_with_nul();
    // Safety: dest has at least |dest_size| bytes allocated, and we won't
    // write any more than that. In addition, we are copying from a slice that
    // includes the nul-terminator, and we are not copying beyond the end of that
    // slice.
    unsafe {
        std::ptr::copy(
            c_str_bytes.as_ptr(),
            std::ptr::from_mut(
                dest.as_mut()
                    .ok_or(anyhow!("couldn't get mutable pointer"))?,
            ),
            c_str_bytes.len(),
        );
    }
    Ok(())
}

/// Redact the given string |s| by retaining only a brief prefix and suffix, to match
/// Desktop's truncateForLogging format.
/// If the string contains unicode, only output one character.
pub fn redact_for_logging(s: &str) -> String {
    if cfg!(debug_assertions) && !cfg!(test) {
        // For debug testing/local builds only, allow the full string.
        s.to_string()
    } else {
        // Take a small number of characters, but fewer if they are non-ascii unicode, as
        // unicode provides a substantially higher amount of information per char.
        // (e.g. four mandarin characters could be a full name)
        let out: String = if s.is_ascii() {
            let n = s.chars().by_ref().count();
            if n <= 4 {
                return s.to_string();
            }
            let mut chars = s.chars();
            let mut out: String = chars.by_ref().take(2).collect();
            out.push_str("...");
            // n - 4 because we're reusing the iterator we took 2 from
            out.extend(chars.skip(n - 4));
            out
        } else {
            if s.chars().count() <= 1 {
                return s.to_string();
            }
            s.chars().take(1).chain("...".chars()).collect()
        };
        out
    }
}

// clippy sees that StaticRegex is "at least 1440 bytes", which could be much
// larger than the &'static str, and thus the enum must be at least 1440 bytes.
// In our case, we only ever have a small handful of these enums (less than 10),
// so the overhead in terms of amount of memory is quite small, and not worth the
// overhead of allocating additional memory, e.g. with a Box.
// https://rust-lang.github.io/rust-clippy/rust-1.91.0/index.html#large_enum_variant
#[allow(clippy::large_enum_variant)]
pub enum AppliesToLine {
    Regex(StaticRegex), // we require a regex to see if the line is of interest
    LiteralPrefix(&'static str), // we can determine if the line is of interest by simple string comparison
}
pub struct RedactionSpec {
    /// Should we redact the given line?
    ///
    /// For cubeb, which may truncate lines that are too long, this should generally identify a
    /// prefix of the relevant log line(s) such that if the regex matches, it is likely we want to
    /// redact the line, and if it fails to match, we know we do not need to (e.g. because cubeb
    /// truncated the line before handing it to us and happened to remove any sensitive content).
    pub applies_to_line: AppliesToLine,
    /// An **ordered** list of the segments to **keep**.
    /// That is to say, for each i from 0 to segments_to_keep.len() - 1,
    /// redact_for_logging the substring between the end of
    /// segments_to_keep[i] and the start of segments_to_keep[i + 1].
    /// Additionally, redact_for_logging the substring from 0 to the start of segments_to_keep[0],
    /// and from the end of the last matching segment to the end of the string.
    /// If segments_to_keep has any elements that do not match, fail conservatively by treating it
    /// as if we have gotten to the last segment in the list, and redact the rest of the string
    /// (An individual segment may not match if the input was truncated by cubeb, for example)
    pub segments_to_keep: Vec<StaticRegex>,
}

impl RedactionSpec {
    pub fn redact_if_matching<'a>(&self, text: Cow<'a, str>) -> Cow<'a, str> {
        // bail early in the likely case that this filter is irrelevant
        let applies = match &self.applies_to_line {
            AppliesToLine::Regex(re) => re.is_match(text.as_bytes()),
            AppliesToLine::LiteralPrefix(s) => text.trim().starts_with(s),
        };
        if !applies {
            return text;
        }

        let mut result = String::new();
        let mut end_of_previous_match = 0;

        for segment_re in self.segments_to_keep.iter() {
            let Some(m) = segment_re.find(&text.as_bytes()[end_of_previous_match..]) else {
                error!("bailing early when redacting string");
                break;
            };
            // Add back the offset
            let start = m.start() + end_of_previous_match;
            let end = m.end() + end_of_previous_match;

            // We have found a substring that is not in |segments_to_keep|, redact it.
            if start != end_of_previous_match {
                result.push_str(&redact_for_logging(&text[end_of_previous_match..start]));
            }
            result.push_str(&text[start..end]);

            end_of_previous_match = end;
        }

        if end_of_previous_match != text.len() {
            // we must also redact the end of the string
            result.push_str(&redact_for_logging(&text[end_of_previous_match..]))
        }

        result.into()
    }
}

pub fn cubeb_redaction_specs() -> &'static Vec<RedactionSpec> {
    // These patterns describe the places friendly_name, device_id, and group_id are logged,
    // for the backends where those are potentially sensitive.
    //
    // The capture groups will be redacted.
    //
    // Note that there's another one that isn't here: cubeb.c logs something like:
    //   LOG("DeviceID: \"%s\"%s\n"
    //       "\tName:\t\"%s\"\n"
    //       ...
    // But we just ignore this line altogether.
    static REDACTION_RES: LazyLock<Vec<RedactionSpec>> = LazyLock::new(|| {
        let mut out = Vec::new();
        if cfg!(target_os = "windows") || cfg!(test) {
            // cubeb_wasapi.cpp: wasapi_find_bt_handsfree_output_device
            out.push(RedactionSpec {
                applies_to_line: AppliesToLine::LiteralPrefix("Found matching device for "),
                segments_to_keep: vec![
                    regex_aot::regex!(r"Found matching device for "),
                    regex_aot::regex!(": "),
                ],
            });
            // cubeb_wasapi.cpp: wasapi_collection_notification_client::OnDefaultDeviceChanged
            // This line ends with a single period. we don't need to redact that, but it's not
            // a helpful part of the name, either.
            out.push(RedactionSpec {
                applies_to_line: AppliesToLine::LiteralPrefix(
                    r"collection: Audio device default changed, id = ",
                ),
                segments_to_keep: vec![
                    regex_aot::regex!(r"collection: Audio device default changed, id = "),
                    regex_aot::regex!(r"\.$"),
                ],
            });
            // cubeb_wasapi.cpp: wasapi_endpoint_notification_client::OnDefaultDeviceChanged
            out.push(RedactionSpec {
                applies_to_line: AppliesToLine::Regex(regex_aot::regex!(
                    r"endpoint: Audio device default changed flow=.* role=.* new_device_id=.*"
                )),
                segments_to_keep: vec![
                    regex_aot::regex!(
                        r"endpoint: Audio device default changed flow=.* role=.* new_device_id="
                    ),
                    regex_aot::regex!(r"\.$"),
                ],
            });
            // cubeb_wasapi.cpp: wasapi_collection_notification_client::OnDeviceStateChanged
            out.push(RedactionSpec {
                applies_to_line: AppliesToLine::LiteralPrefix(
                    r"collection: Audio device state changed, id = ",
                ),
                segments_to_keep: vec![
                    regex_aot::regex!(r"collection: Audio device state changed, id = "),
                    regex_aot::regex!(", state =.*"),
                ],
            });
        }
        if cfg!(target_os = "macos") || cfg!(test) {
            // cubeb-coreaudio-rs src/backend/mod.rs audiounit_get_devices_of_type
            // See unit test redact_cubeb_strings for an example
            out.push(RedactionSpec {
                applies_to_line: AppliesToLine::Regex(regex_aot::regex!(r"Device \d+ \(.*")),
                segments_to_keep: vec![
                    regex_aot::regex!(r"Device \d+ \("),
                    regex_aot::regex!(r"\) has \d+.*channels"),
                ],
            });
            out.push(RedactionSpec {
                applies_to_line: AppliesToLine::LiteralPrefix(
                    "Cannot get the channel count for device ",
                ),
                segments_to_keep: vec![
                    regex_aot::regex!(r"Cannot get the channel count for device \d+ \("),
                    regex_aot::regex!(r"\)\. Ignored\."),
                ],
            });
            // cubeb-coreaudio-rs src/backend/mod.rs should_block_vpio_for_device_pair
            // See unit test redact_cubeb_strings for an example
            out.push(RedactionSpec {
                applies_to_line: AppliesToLine::Regex(regex_aot::regex!("(Input|Output) uid=")),
                segments_to_keep: vec![
                    regex_aot::regex!("(Input|Output) uid=\""),
                    regex_aot::regex!("\", model_uid=\""),
                    regex_aot::regex!("\", transport_type=.*, source=.*, source_name=\""),
                    regex_aot::regex!("\", name=\""),
                    regex_aot::regex!("\", manufacturer=\".*\""),
                ],
            });
        }
        if cfg!(target_os = "linux") || cfg!(test) {
            // cubeb-pulse-rs context.rs server_info_cb::sink_info_cb
            out.push(RedactionSpec {
                applies_to_line: AppliesToLine::LiteralPrefix(
                    "PulseAudio default sink info: name=",
                ),
                segments_to_keep: vec![
                    regex_aot::regex!("PulseAudio default sink info: name="),
                    regex_aot::regex!(", description="),
                    regex_aot::regex!(r", driver=.*, latency=\d+"),
                ],
            });
            // cubeb-pulse-rs context.rs server_info_cb
            out.push(RedactionSpec {
                applies_to_line: AppliesToLine::Regex(regex_aot::regex!(
                    r"PulseAudio server info: server_name=.*, default_sink_name="
                )),
                segments_to_keep: vec![
                    regex_aot::regex!(
                        r"PulseAudio server info: server_name=.*, default_sink_name="
                    ),
                    regex_aot::regex!(", default_source_name="),
                ],
            });
        }
        out
    });

    &REDACTION_RES
}

pub fn do_cubeb_redactions(text: Cow<str>) -> Cow<str> {
    let mut redacted = text;
    for re in cubeb_redaction_specs().iter() {
        redacted = re.redact_if_matching(redacted);
    }
    redacted
}

#[cfg(test)]
mod audio_device_module_tests {
    #[cfg(target_os = "linux")]
    use cubeb_core::DeviceType;

    use super::*;
    #[test]
    // Verify that extremely long strings are properly truncated and
    // nul-terminated
    fn copy_and_truncate_long_string() {
        let data = vec![0xaau8; 10];
        let src = String::from_iter(['A'; 20]); // longer than data
        let out = webrtc::ptr::Borrowed::from_ptr(data.as_ptr());
        copy_and_truncate_string(&src, out, data.len()).unwrap();
        let mut expected = vec![0x41u8; 9]; // 'A'
        expected.push(0);
        assert_eq!(data, expected);
    }

    #[test]
    // Ensure that we do not read past the end of `src`
    fn copy_and_truncate_short_string() {
        let data = vec![0xaau8; 10];
        let src = String::from_iter(['A'; 4]); // shorter than data
        let out = webrtc::ptr::Borrowed::from_ptr(data.as_ptr());
        copy_and_truncate_string(&src, out, data.len()).unwrap();
        let expected = vec![0x41u8, 0x41, 0x41, 0x41, 0x0, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa];
        assert_eq!(data, expected);
    }

    #[test]
    // Check for off-by-one errors
    fn copy_and_truncate_max_len_string() {
        let data = vec![0xaau8; 10];
        let src = String::from_iter(['A'; 10]); // equal length to data
        let out = webrtc::ptr::Borrowed::from_ptr(data.as_ptr());
        copy_and_truncate_string(&src, out, data.len()).unwrap();
        let mut expected = vec![0x41u8; 9]; // 'A'
        expected.push(0);
        assert_eq!(data, expected);
    }

    #[test]
    // Check for off-by-one errors
    fn copy_and_truncate_barely_short_string() {
        let data = vec![0xaau8; 10];
        let src = String::from_iter(['A'; 9]); // one shorter than data
        let out = webrtc::ptr::Borrowed::from_ptr(data.as_ptr());
        copy_and_truncate_string(&src, out, data.len()).unwrap();
        let mut expected = vec![0x41u8; 9]; // 'A'
        expected.push(0);
        assert_eq!(data, expected);
    }

    #[test]
    // Check for overwrite errors
    fn copy_no_overwrite() {
        let data = vec![0xaau8; 10];
        let src = String::from_iter(['A'; 20]); // longer than data
        let out = webrtc::ptr::Borrowed::from_ptr(data.as_ptr());
        // State that data has one fewer byte than it actually does to make sure
        // the function doesn't write past the end.
        copy_and_truncate_string(&src, out, data.len() - 1).unwrap();
        let mut expected = vec![0x41u8; 8]; // 'A'
        expected.push(0);
        expected.push(0xaa);
        assert_eq!(data, expected);
    }

    #[test]
    // Verify that a string with internal nul characters is handled gracefully.
    fn string_with_nuls() {
        let data = vec![0xaau8; 10];
        let src = "a\0b";
        let out = webrtc::ptr::Borrowed::from_ptr(data.as_ptr());
        assert!(copy_and_truncate_string(src, out, data.len() - 1).is_err());
        // data should be untouched
        assert_eq!(data, vec![0xaau8; 10]);
    }

    #[test]
    // Verify that a null dest pointer is handled gracefully
    fn null_ptr() {
        let src = "AA";
        let out = webrtc::ptr::Borrowed::null();
        assert!(copy_and_truncate_string(src, out, 5).is_err());
    }

    #[test]
    fn redaction_tests() {
        assert_eq!(redact_for_logging("0123456789"), "01...89");
        assert_eq!(redact_for_logging("0123"), "0123");
        assert_eq!(redact_for_logging("0"), "0");
        assert_eq!(redact_for_logging("你好"), "你..."); // ni hao (hello)
        assert_eq!(redact_for_logging("你"), "你");
        // This is not necessarily behavior we want to enforce, but the test is here for
        // documentation of the limitations of this implementation:
        // the string y̆, which looks like one character to humans, is represented as
        // two Unicode Scalar Values: y and \u{0306}.
        // Rust's standard library does not provide functionality to iterate by "grapheme clusters."
        //
        // While we could get such a library from crates.io, it would require us to build in a
        // (large) unicode table to the compiled library.
        assert_eq!(redact_for_logging("y̆ is from rust str docs"), "y...");
    }

    #[test]
    fn redaction_regex_tests() {
        let re1 = RedactionSpec {
            applies_to_line: AppliesToLine::Regex(regex_aot::regex!(r"Device \d+ \(.*")),
            segments_to_keep: vec![
                regex_aot::regex!(r"Device \d+ \("),
                regex_aot::regex!(r"\) has \d+.*channels"),
            ],
        };

        assert_eq!(
            re1.redact_if_matching(
                "Device 12345 (My Super Sensitive Name) has 2 INPUT-channels".into()
            ),
            "Device 12345 (My...me) has 2 INPUT-channels"
        );

        // Should only redact matching strings
        assert_eq!(
            re1.redact_if_matching("Some other string with My Super Sensitive Name".into()),
            "Some other string with My Super Sensitive Name"
        );

        let re2 = RedactionSpec {
            applies_to_line: AppliesToLine::LiteralPrefix("Found matching device for "),
            segments_to_keep: vec![
                regex_aot::regex!(r"Found matching device for "),
                regex_aot::regex!(": "),
            ],
        };
        assert_eq!(
            re2.redact_if_matching(
                "Found matching device for My Super Sensitive Name: My Super Sensitive Name".into()
            ),
            "Found matching device for My...me: My...me"
        );

        assert_eq!(
            re2.redact_if_matching(
                "Found matching device for My Super Sensitive Name: My Other Sensitive Name".into()
            ),
            "Found matching device for My...me: My...me"
        );

        let re3 = RedactionSpec {
            applies_to_line: AppliesToLine::LiteralPrefix(
                r"collection: Audio device default changed, id = ",
            ),
            segments_to_keep: vec![
                regex_aot::regex!(r"collection: Audio device default changed, id = "),
                regex_aot::regex!(",$"),
            ],
        };
        assert_eq!(
            re3.redact_if_matching(
                // note that the comma at the end is not preserved
                "collection: Audio device default changed, id = TestOneTwo,".into()
            ),
            // Note that there's no comma at the end
            "collection: Audio device default changed, id = Te...wo,"
        );

        assert_eq!(
            re3.redact_if_matching(
                " collection: Audio device default changed, id = TestOneTwo,".into()
            ),
            " collection: Audio device default changed, id = Te...wo,"
        );

        let re4 = RedactionSpec {
            applies_to_line: AppliesToLine::Regex(regex_aot::regex!(r".* is the sensitive part")),
            segments_to_keep: vec![regex_aot::regex!(r" is the sensitive part")],
        };
        assert_eq!(
            re4.redact_if_matching("Sensitive Name is the sensitive part".into()),
            "Se...me is the sensitive part"
        );
    }

    #[test]
    fn redaction_regex_failure() {
        let re1 = RedactionSpec {
            applies_to_line: AppliesToLine::Regex(regex_aot::regex!(r"Device \d+ \(.*")),
            segments_to_keep: vec![
                regex_aot::regex!(r"Device \d+ \("),
                // Deliberately missing a space before "has"
                regex_aot::regex!(r"\)has \d+.*channels"),
            ],
        };
        assert_eq!(
            re1.redact_if_matching(
                "Device 12345 (My Super Sensitive Name) has 2 INPUT-channels".into()
            ),
            // We'll take what we know is safe and truncate the rest.
            "Device 12345 (My...ls"
        );
    }

    #[test]
    fn redact_unicode() {
        let re1 = RedactionSpec {
            applies_to_line: AppliesToLine::Regex(regex_aot::regex!(
                r"endpoint: Audio device default changed flow=.* role=.* new_device_id=.*"
            )),
            segments_to_keep: vec![regex_aot::regex!(
                r"endpoint: Audio device default changed flow=.* role=.* new_device_id="
            )],
        };

        // Should not affect the y̆
        assert_eq!(
            re1.redact_if_matching("endpoint: Audio device default changed flow=y̆ is from rust str docs role=console new_device_id=Sensitive ASCII".into()),
           "endpoint: Audio device default changed flow=y̆ is from rust str docs role=console new_device_id=Se...II"
        );

        let re2 = RedactionSpec {
            applies_to_line: AppliesToLine::LiteralPrefix("Found matching device for "),
            segments_to_keep: vec![
                regex_aot::regex!(r"Found matching device for "),
                regex_aot::regex!(": "),
            ],
        };
        // Presence of unicode should cause that string to be redacted more strictly
        assert_eq!(
            re2.redact_if_matching(
                "Found matching device for My é: My Other Sensitive Name".into()
            ),
            "Found matching device for M...: My...me"
        );

        // Should only apply stricter redaction to the one string
        assert_eq!(
            re2.redact_if_matching(
                "Found matching device for Édouard Manet: My Other Sensitive Name".into()
            ),
            "Found matching device for É...: My...me"
        );
    }

    // Makes sure that a "." doesn't lead to us cutting in the middle of a character boundary
    #[test]
    fn redact_regex_no_char_boundary_bug() {
        let re = RedactionSpec {
            applies_to_line: AppliesToLine::LiteralPrefix("A"),
            segments_to_keep: vec![regex_aot::regex!("A.")],
        };
        assert_eq!(re.redact_if_matching("A你好你好".into()), "A你好...");
    }

    #[test]
    fn redact_cubeb_strings() {
        assert_eq!(
            // This line shouldn't be redacted at all, so it shouldn't be affected by the grapheme cluster issue
            do_cubeb_redactions("Some Line Not Matching With Unicode y̆ 你好".into()),
            "Some Line Not Matching With Unicode y̆ 你好"
        );

        assert_eq!(
            do_cubeb_redactions(
                "Found matching device for Friendly Name: Other Friendly Name".into()
            ),
            "Found matching device for Fr...me: Ot...me"
        );

        assert_eq!(
            do_cubeb_redactions("collection: Audio device default changed, id = {0.0.0.00000000}.{c9ddf6ba-2fad-406b-bddf-bded5b431800}".into()),
            "collection: Audio device default changed, id = {0...0}"
        );

        assert_eq!(
            do_cubeb_redactions("collection: Audio device state changed, id = {0.0.0.00000000}.{c9ddf6ba-2fad-406b-bddf-bded5b431800}, state = 4.".into()),
            "collection: Audio device state changed, id = {0...0}, state = 4."
        );

        // This line is cut off early, but that should still work.
        assert_eq!(
            do_cubeb_redactions(
                "collection: Audio device state changed, id = {0.0.0.00000000}.{c9ddf6ba-".into()
            ),
            "collection: Audio device state changed, id = {0...a-"
        );

        assert_eq!(
            do_cubeb_redactions(
                "endpoint: Audio device default changed flow=1 role=2 new_device_id={0.0.0.00000000}.{c9ddf6ba-2fad-406b-bddf-bded5b431800}.".into()
            ),
            "endpoint: Audio device default changed flow=1 role=2 new_device_id={0...0}."
        );

        assert_eq!(
            do_cubeb_redactions("Device 92 (MacBook Pro Microphone) has 1 INPUT-channels".into()),
            "Device 92 (Ma...ne) has 1 INPUT-channels"
        );

        assert_eq!(
            do_cubeb_redactions(
                "Cannot get the channel count for device 92 (MacBook Pro Microphone). Ignored."
                    .into()
            ),
            "Cannot get the channel count for device 92 (Ma...ne). Ignored."
        );

        // If we see a cut-off line in the middle of a regex that should truncate the rest of the line
        assert_eq!(
            do_cubeb_redactions(
                "Cannot get the channel count for device 92 (MacBook Pro Microphone). Igno".into()
            ),
            "Cannot get the channel count for device 92 (Ma...no"
        );

        assert_eq!(
            do_cubeb_redactions(r#"Output uid="BuiltInHeadphoneOutputDevice", model_uid="Codec Output", transport_type="bltn", source="hdpn", source_name="External Headphones", name="External Headphones", manufacturer="Apple Inc.""#.into()),
           r#"Output uid="Bu...ce", model_uid="Co...ut", transport_type="bltn", source="hdpn", source_name="Ex...es", name="Ex...es", manufacturer="Apple Inc.""#
        );

        assert_eq!(do_cubeb_redactions("PulseAudio server info: server_name=PulseAudio (on PipeWire 1.4.2), server_version=15.0.0, default_sink_name=alsa_output.pci-0000_64_00.6.HiFi__Headphones__sink, default_source_name=alsa_input.pci-0000_64_00.6.HiFi__Mic1__source".into()),
        "PulseAudio server info: server_name=PulseAudio (on PipeWire 1.4.2), server_version=15.0.0, default_sink_name=al...nk, default_source_name=al...ce");

        assert_eq!(do_cubeb_redactions("PulseAudio default sink info: name=alsa_output.pci-0000_64_00.6.HiFi__Speaker__sink, description=Ryzen HD Audio Controller Speaker, driver=PipeWire, latency=0".into()),
        "PulseAudio default sink info: name=al...nk, description=Ry...er, driver=PipeWire, latency=0");

        // Note the leading whitespace
        assert_eq!(do_cubeb_redactions(" PulseAudio default sink info: name=alsa_output.pci-0000_64_00.6.HiFi__Speaker__sink, description=Ryzen HD Audio Controller Speaker, driver=PipeWire, latency=0".into()),
                   " PulseAudio default sink info: name=al...nk, description=Ry...er, driver=PipeWire, latency=0");
    }

    #[test]
    fn extract_names_handles_normal() {
        let devs = DeviceCollectionWrapper {
            device_collection: vec![
                MinimalDeviceInfo {
                    devid: std::ptr::null(),
                    device_id: Some("devid1".to_string()),
                    friendly_name: "Device 1".to_string(),
                    #[cfg(target_os = "linux")]
                    device_type: DeviceType::INPUT,
                    preferred: DevicePref::all(),
                    state: DeviceState::Enabled,
                },
                MinimalDeviceInfo {
                    devid: std::ptr::null(),
                    device_id: Some("devid2".to_string()),
                    friendly_name: "Device 2".to_string(),
                    #[cfg(target_os = "linux")]
                    device_type: DeviceType::INPUT,
                    preferred: DevicePref::empty(),
                    state: DeviceState::Enabled,
                },
            ],
        };
        let names = DeviceCollectionWrapper::extract_names(&devs);

        #[cfg(not(target_os = "windows"))]
        assert_eq!(
            names,
            vec![
                Some(AudioDevice {
                    name: "default (Device 1)".to_string(),
                    unique_id: "devid1".to_string(),
                    i18n_key: "".to_string(),
                }),
                Some(AudioDevice {
                    name: "Device 1".to_string(),
                    unique_id: "devid1".to_string(),
                    i18n_key: "".to_string(),
                }),
                Some(AudioDevice {
                    name: "Device 2".to_string(),
                    unique_id: "devid2".to_string(),
                    i18n_key: "".to_string(),
                })
            ]
        );

        #[cfg(target_os = "windows")]
        assert_eq!(
            names,
            vec![
                Some(AudioDevice {
                    name: "Default - Device 1".to_string(),
                    unique_id: "devid1".to_string(),
                    i18n_key: "".to_string(),
                }),
                Some(AudioDevice {
                    name: "Communication - Device 1".to_string(),
                    unique_id: "devid1".to_string(),
                    i18n_key: "".to_string(),
                }),
                Some(AudioDevice {
                    name: "Device 1".to_string(),
                    unique_id: "devid1".to_string(),
                    i18n_key: "".to_string(),
                }),
                Some(AudioDevice {
                    name: "Device 2".to_string(),
                    unique_id: "devid2".to_string(),
                    i18n_key: "".to_string(),
                })
            ]
        );
    }

    #[test]
    fn extract_names_handles_no_preferred_device() {
        let devs = DeviceCollectionWrapper {
            device_collection: vec![
                MinimalDeviceInfo {
                    devid: std::ptr::null(),
                    device_id: Some("devid1".to_string()),
                    friendly_name: "Device 1".to_string(),
                    #[cfg(target_os = "linux")]
                    device_type: DeviceType::INPUT,
                    preferred: DevicePref::empty(),
                    state: DeviceState::Enabled,
                },
                MinimalDeviceInfo {
                    devid: std::ptr::null(),
                    device_id: Some("devid2".to_string()),
                    friendly_name: "Device 2".to_string(),
                    #[cfg(target_os = "linux")]
                    device_type: DeviceType::INPUT,
                    preferred: DevicePref::empty(),
                    state: DeviceState::Enabled,
                },
            ],
        };
        let names = DeviceCollectionWrapper::extract_names(&devs);

        assert_eq!(
            names,
            vec![
                None,
                // Windows expects an extra communication device.
                #[cfg(target_os = "windows")]
                None,
                Some(AudioDevice {
                    name: "Device 1".to_string(),
                    unique_id: "devid1".to_string(),
                    i18n_key: "".to_string(),
                }),
                Some(AudioDevice {
                    name: "Device 2".to_string(),
                    unique_id: "devid2".to_string(),
                    i18n_key: "".to_string(),
                })
            ]
        )
    }
}
