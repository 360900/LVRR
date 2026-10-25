//! Per-game workarounds keyed on the game's executable name, in the spirit
//! of GPU driver application profiles. Environment variables always override
//! the corresponding quirk.

use std::sync::OnceLock;

#[derive(Debug, Default, Clone, Copy)]
pub struct Quirks {
    /// Don't expose generic trackers to this game. Trackers identify as Vive
    /// Trackers, which some games sniff to pick a controller scheme (e.g.
    /// SUPERHOT VR switches to its Vive scheme - where dropping items
    /// requires a trackpad - when it sees one).
    pub no_generic_trackers: bool,

    /// Report linear velocities computed from position deltas instead of the
    /// runtime's. Some runtimes under-report linear velocity (measured ~0.43x
    /// on WiVRn), which makes throwing objects nearly impossible in games that
    /// use controller velocity for it. Position-derived velocities stay
    /// correct even on runtimes without the problem, just redundant.
    /// Env override: XRIZER_SYNTHESIZED_VELOCITY=1/0.
    pub synthesized_velocity: bool,

    /// Treat 8-bit UNORM textures submitted with ColorSpace_Auto as linear. The OpenVR header
    /// defines Auto as gamma for them, which games rendering sRGB encoded values into UNORM
    /// textures rely on (D3D11 TYPELESS textures arrive as UNORM through DXVK), but a game
    /// writing linear values looks too dark that way (reportedly Outer Wilds with NomaiVR).
    /// Env override: XRIZER_LINEAR_UNORM=1/0.
    pub linear_unorm: bool,
}

/// Tracking glitches can teleport the controller for a frame; without a cap, a
/// synthesized velocity happily turns that into a rocket-powered throw.
/// Slightly above the fastest plausible human hand speed.
pub const MAX_SYNTHESIZED_SPEED: f32 = 12.0;

/// Whether to report linear velocities computed from position deltas, honouring
/// the XRIZER_SYNTHESIZED_VELOCITY override.
pub fn synthesized_velocity() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        env_override("XRIZER_SYNTHESIZED_VELOCITY").unwrap_or(get().synthesized_velocity)
    })
}

/// Whether 8-bit UNORM textures with ColorSpace_Auto hold linear rather than gamma encoded
/// values, honouring the XRIZER_LINEAR_UNORM override.
pub fn linear_unorm() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_override("XRIZER_LINEAR_UNORM").unwrap_or(get().linear_unorm))
}

/// A boolean environment override, if one is set. Anything other than 1/true
/// disables the quirk, so a game profile can be turned off as well as on.
pub(crate) fn env_override(var: &str) -> Option<bool> {
    let value = std::env::var(var).ok()?;
    let value = value.trim();
    Some(value == "1" || value.eq_ignore_ascii_case("true"))
}

pub fn get() -> Quirks {
    static QUIRKS: OnceLock<Quirks> = OnceLock::new();
    *QUIRKS.get_or_init(|| {
        let Some(exe) = game_exe_name() else {
            return Quirks::default();
        };

        let quirks = match exe.to_lowercase().as_str() {
            "superhotvr.exe" => Quirks {
                no_generic_trackers: true,
                ..Default::default()
            },
            // Half-Life 2: VR Mod (and episodes) computes throws from hand velocity
            "hl2.exe" => Quirks {
                synthesized_velocity: true,
                ..Default::default()
            },
            _ => Quirks::default(),
        };

        if quirks.no_generic_trackers || quirks.synthesized_velocity || quirks.linear_unorm {
            log::info!("Applying game quirks for {exe}: {quirks:?}");
        }
        quirks
    })
}

/// The basename of the game's executable. Wine rewrites argv[0] to the
/// Windows path of the .exe, so this works for Proton games too.
fn game_exe_name() -> Option<String> {
    let cmdline = std::fs::read_to_string("/proc/self/cmdline")
        .ok()
        .and_then(|args| {
            args.split('\0').next().map(|argv0| {
                argv0
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(argv0)
                    .to_string()
            })
        });

    cmdline
        .filter(|name| !name.is_empty())
        .or_else(|| {
            // comm is truncated to 15 characters, but it's better than nothing.
            std::fs::read_to_string("/proc/self/comm")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    #[test]
    fn exe_name_resolves() {
        // The test runner is a normal Linux process, so this should always
        // produce something.
        assert!(super::game_exe_name().is_some());
    }
}
