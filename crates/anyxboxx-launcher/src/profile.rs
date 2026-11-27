//! Launch profiles: the data of `profiles.toml` and the choice of one profile for a host and a GPU.
//! The data is embedded in the binary, so the launcher never reads a file it cannot verify.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::host::{GpuVendor, Host};

/// The profile data, compiled in.
pub const PROFILES_TOML: &str = include_str!("../profiles.toml");

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("profiles.toml does not parse: {0}")]
    Parse(String),
    #[error("profile '{0}': unknown {1} '{2}'")]
    UnknownSelector(String, &'static str, String),
    #[error("profile '{0}': variable '{1}' is not a VIctory or Proton setting")]
    BadKey(String, String),
    #[error("profile '{0}': pending key '{1}' is not set in its env table")]
    PendingNotSet(String, String),
    #[error("no profile matches the host: the 'any/any' fallback is missing")]
    NoFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Measured,
    Unmeasured,
}

#[derive(Debug, Clone, Deserialize)]
struct ProfileFile {
    profile: Vec<RawProfile>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawProfile {
    id: String,
    name: String,
    os: String,
    gpu: String,
    status: Status,
    #[serde(default)]
    note: String,
    #[serde(default)]
    pending: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// One profile, validated: its selectors are known and its keys are VIctory settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub os: Selector<Host>,
    pub gpu: Selector<GpuVendor>,
    pub status: Status,
    pub note: String,
    /// Keys whose values wait for the maintainer's confirmation on Windows.
    pub pending: Vec<String>,
    /// Variables the profile sets, in key order.
    pub env: BTreeMap<String, String>,
}

/// What a profile applies to: one value, or every value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector<T> {
    Any,
    Only(T),
}

impl<T: PartialEq> Selector<T> {
    fn matches(&self, value: &T) -> bool {
        match self {
            Selector::Any => true,
            Selector::Only(only) => only == value,
        }
    }

    fn is_specific(&self) -> bool {
        matches!(self, Selector::Only(_))
    }
}

/// The profiles of `profiles.toml`, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profiles(Vec<Profile>);

impl Profiles {
    /// Parses and validates the given TOML text.
    pub fn parse(text: &str) -> Result<Self, ProfileError> {
        let file: ProfileFile =
            toml::from_str(text).map_err(|e| ProfileError::Parse(e.to_string()))?;
        let profiles = file
            .profile
            .into_iter()
            .map(validate)
            .collect::<Result<Vec<_>, _>>()?;
        let profiles = Profiles(profiles);
        if !profiles
            .0
            .iter()
            .any(|p| !p.os.is_specific() && !p.gpu.is_specific())
        {
            return Err(ProfileError::NoFallback);
        }
        Ok(profiles)
    }

    /// The profiles compiled into the launcher.
    pub fn embedded() -> Result<Self, ProfileError> {
        Self::parse(PROFILES_TOML)
    }

    #[cfg(test)]
    pub fn all(&self) -> &[Profile] {
        &self.0
    }

    #[cfg(test)]
    pub fn by_id(&self, id: &str) -> Option<&Profile> {
        self.0.iter().find(|p| p.id == id)
    }

    /// The most specific profile for the host and the GPU: a profile naming both selectors beats
    /// one naming one, which beats the fallback; a tie goes to the first one in the file.
    pub fn select(&self, host: Host, gpu: GpuVendor) -> &Profile {
        let mut best: Option<(usize, &Profile)> = None;
        for profile in &self.0 {
            if !profile.os.matches(&host) || !profile.gpu.matches(&gpu) {
                continue;
            }
            let score =
                usize::from(profile.os.is_specific()) + usize::from(profile.gpu.is_specific());
            if best.is_none_or(|(top, _)| score > top) {
                best = Some((score, profile));
            }
        }
        // `parse` guarantees an any/any profile, which matches everything.
        best.map_or(&self.0[0], |(_, profile)| profile)
    }
}

fn validate(raw: RawProfile) -> Result<Profile, ProfileError> {
    let os = parse_selector(&raw.id, "os", &raw.os, host_from_name)?;
    let gpu = parse_selector(&raw.id, "gpu", &raw.gpu, vendor_from_name)?;
    for key in raw.env.keys() {
        if !(key.starts_with("VICTORY_") || key.starts_with("VKD3D_")) {
            return Err(ProfileError::BadKey(raw.id.clone(), key.clone()));
        }
    }
    for key in &raw.pending {
        if !raw.env.contains_key(key) {
            return Err(ProfileError::PendingNotSet(raw.id.clone(), key.clone()));
        }
    }
    Ok(Profile {
        id: raw.id,
        name: raw.name,
        os,
        gpu,
        status: raw.status,
        note: raw.note,
        pending: raw.pending,
        env: raw.env,
    })
}

fn parse_selector<T>(
    id: &str,
    field: &'static str,
    value: &str,
    convert: fn(&str) -> Option<Option<T>>,
) -> Result<Selector<T>, ProfileError> {
    match convert(value) {
        Some(None) => Ok(Selector::Any),
        Some(Some(only)) => Ok(Selector::Only(only)),
        None => Err(ProfileError::UnknownSelector(
            id.to_string(),
            field,
            value.to_string(),
        )),
    }
}

fn host_from_name(name: &str) -> Option<Option<Host>> {
    match name {
        "any" => Some(None),
        "windows" => Some(Some(Host::Windows)),
        "linux" => Some(Some(Host::WineOrProton)),
        _ => None,
    }
}

fn vendor_from_name(name: &str) -> Option<Option<GpuVendor>> {
    match name {
        "any" => Some(None),
        "nvidia" => Some(Some(GpuVendor::Nvidia)),
        "amd" => Some(Some(GpuVendor::Amd)),
        "intel" => Some(Some(GpuVendor::Intel)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded() -> Profiles {
        Profiles::embedded().expect("the embedded profiles.toml is valid")
    }

    fn ids_selected(host: Host, gpu: GpuVendor) -> String {
        embedded().select(host, gpu).id.clone()
    }

    #[test]
    fn embedded_profiles_parse_with_unique_ids() {
        let profiles = embedded();
        let mut ids: Vec<&str> = profiles.all().iter().map(|p| p.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), profiles.all().len());
        assert_eq!(profiles.all().len(), 5);
    }

    #[test]
    fn windows_nvidia_selects_the_maintainer_profile() {
        assert_eq!(
            ids_selected(Host::Windows, GpuVendor::Nvidia),
            "windows-nvidia"
        );
    }

    #[test]
    fn linux_amd_selects_the_proton_profile() {
        assert_eq!(
            ids_selected(Host::WineOrProton, GpuVendor::Amd),
            "linux-amd"
        );
    }

    #[test]
    fn windows_amd_selects_its_measured_profile() {
        assert_eq!(ids_selected(Host::Windows, GpuVendor::Amd), "windows-amd");
        assert_eq!(
            embedded().by_id("windows-amd").map(|p| p.status),
            Some(Status::Measured)
        );
    }

    #[test]
    fn intel_selects_the_intel_profile_on_any_host() {
        assert_eq!(ids_selected(Host::Windows, GpuVendor::Intel), "intel");
        assert_eq!(ids_selected(Host::WineOrProton, GpuVendor::Intel), "intel");
    }

    #[test]
    fn unknown_gpu_or_host_falls_back_to_the_defaults() {
        assert_eq!(ids_selected(Host::Windows, GpuVendor::Unknown), "other");
        assert_eq!(ids_selected(Host::WineOrProton, GpuVendor::Nvidia), "other");
        assert_eq!(
            ids_selected(Host::WineOrProton, GpuVendor::Other(0x1414)),
            "other"
        );
    }

    #[test]
    fn fallback_profiles_set_nothing() {
        let profiles = embedded();
        for id in ["intel", "other"] {
            let profile = profiles.by_id(id).expect("profile exists");
            assert!(profile.env.is_empty(), "{id} must be VIctory's defaults");
            assert_eq!(profile.status, Status::Unmeasured);
        }
    }

    fn sorted_env(id: &str) -> Vec<(String, String)> {
        let profile = embedded().by_id(id).expect("profile").clone();
        assert!(profile.pending.is_empty(), "{id}");
        let mut actual: Vec<(String, String)> = profile
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        actual.sort();
        actual
    }

    fn sorted_table(table: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut expected: Vec<(String, String)> = table
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        expected.sort();
        expected
    }

    #[test]
    fn windows_amd_is_exactly_the_tuned_set() {
        assert_eq!(
            sorted_env("windows-amd"),
            sorted_table(anyxboxx::run::TUNED)
        );
    }

    #[test]
    fn windows_nvidia_is_exactly_the_cli_diffbase_set_and_measured() {
        assert_eq!(
            sorted_env("windows-nvidia"),
            sorted_table(anyxboxx::run::DIFFBASE)
        );
        let nvidia = embedded().by_id("windows-nvidia").expect("profile").clone();
        assert_eq!(nvidia.status, Status::Measured);
        assert!(!nvidia.env.contains_key(anyxboxx::run::DIFFBASE_DROPS));
    }

    #[test]
    fn linux_amd_is_exactly_the_cli_linux_amd_set() {
        use anyxboxx::run::LINUX_AMD;
        let profile = embedded().by_id("linux-amd").expect("profile").clone();
        let mut expected: Vec<(String, String)> = LINUX_AMD
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let mut actual: Vec<(String, String)> = profile
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        expected.sort();
        actual.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn linux_amd_has_no_pending_switch_and_no_tuned_only_key() {
        let profile = embedded().by_id("linux-amd").expect("profile").clone();
        assert!(profile.pending.is_empty());
        // The relay's own late-wait stage (3) is kept: the old Linux set's VICTORY_LATE_WAIT=0 is gone.
        assert!(!profile.env.contains_key("VICTORY_LATE_WAIT"));
        assert_eq!(
            profile.env.get("VICTORY_RELAY").map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn parsing_rejects_unknown_selectors() {
        let text =
            "[[profile]]\nid=\"x\"\nname=\"x\"\nos=\"plan9\"\ngpu=\"any\"\nstatus=\"measured\"\n";
        assert!(matches!(
            Profiles::parse(text),
            Err(ProfileError::UnknownSelector(_, "os", _))
        ));
    }

    #[test]
    fn parsing_rejects_keys_that_are_not_victory_settings() {
        let text = "[[profile]]\nid=\"x\"\nname=\"x\"\nos=\"any\"\ngpu=\"any\"\nstatus=\"unmeasured\"\n[profile.env]\nPATH=\"x\"\n";
        assert!(matches!(
            Profiles::parse(text),
            Err(ProfileError::BadKey(_, _))
        ));
    }

    #[test]
    fn parsing_rejects_a_pending_key_that_is_not_set() {
        let text = "[[profile]]\nid=\"x\"\nname=\"x\"\nos=\"any\"\ngpu=\"any\"\nstatus=\"unmeasured\"\npending=[\"VICTORY_X\"]\n[profile.env]\n";
        assert!(matches!(
            Profiles::parse(text),
            Err(ProfileError::PendingNotSet(_, _))
        ));
    }

    #[test]
    fn parsing_requires_a_fallback_profile() {
        let text = "[[profile]]\nid=\"x\"\nname=\"x\"\nos=\"windows\"\ngpu=\"nvidia\"\nstatus=\"unmeasured\"\n[profile.env]\n";
        assert!(matches!(
            Profiles::parse(text),
            Err(ProfileError::NoFallback)
        ));
    }

    #[test]
    fn specific_profile_beats_the_fallback_and_ties_keep_file_order() {
        let text = "[[profile]]\nid=\"fallback\"\nname=\"f\"\nos=\"any\"\ngpu=\"any\"\nstatus=\"unmeasured\"\n[profile.env]\n\
                    [[profile]]\nid=\"first\"\nname=\"a\"\nos=\"any\"\ngpu=\"nvidia\"\nstatus=\"unmeasured\"\n[profile.env]\n\
                    [[profile]]\nid=\"second\"\nname=\"b\"\nos=\"any\"\ngpu=\"nvidia\"\nstatus=\"unmeasured\"\n[profile.env]\n";
        let profiles = Profiles::parse(text).expect("valid");
        assert_eq!(
            profiles.select(Host::Windows, GpuVendor::Nvidia).id,
            "first"
        );
        assert_eq!(
            profiles.select(Host::Windows, GpuVendor::Amd).id,
            "fallback"
        );
    }
}
