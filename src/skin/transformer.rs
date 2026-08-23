//! Port of `Skinning/SkinTransformer.cs` (+`ISkinTransformer.cs`) and the
//! current-skin resolution of `Skinning/SkinManager.cs`.
//!
//! lazer resolves a drawable's skin queries through a chain of
//! `SkinProvidingContainer`s: the user skin (wrapped in the ruleset's
//! `LegacySkinTransformer`) first, the game default skin as the outer
//! fallback. [`ResolvedSkin`] is that chain: the user's
//! [`LegacySkin`] when one is loaded (every lookup asks it first), then
//! the built-in [`ArgonSkin`].

use std::path::Path;

use super::argon::ArgonSkin;
use super::configuration::SkinConfiguration;
use super::legacy::LegacySkin;
use super::lookup::SkinLookup;
use super::texture::SkinTexture;
use super::{Skin, SkinTextureSource};

/// The skin a render (or host) resolves skins against: the user skin
/// with the default skin as fallback, lazer's container chain collapsed
/// into one object.
pub struct ResolvedSkin {
    legacy: Option<LegacySkin>,
    builtin: ArgonSkin,
}

impl ResolvedSkin {
    /// The user legacy skin, when one is loaded.
    pub fn legacy(&self) -> Option<&LegacySkin> {
        self.legacy.as_ref()
    }

    /// Whether legacy (stable-format) resources should drive the visuals
    /// - lazer's `skin is LegacySkin` checks, the gate legacy-skinned
    /// drawables use.
    pub fn is_legacy(&self) -> bool {
        self.legacy.is_some()
    }
}

impl Skin for ResolvedSkin {
    fn name(&self) -> &str {
        match &self.legacy {
            Some(l) => l.name(),
            None => self.builtin.name(),
        }
    }

    fn configuration(&self) -> &SkinConfiguration {
        match &self.legacy {
            Some(l) => l.configuration(),
            None => self.builtin.configuration(),
        }
    }

    fn is_legacy(&self) -> bool {
        self.legacy.is_some()
    }

    /// Ask the user skin first; on a miss fall back to the default skin
    /// (`SkinTransformer`'s pass-through per layer).
    fn get_texture(&self, name: &str) -> Option<SkinTexture> {
        match &self.legacy {
            Some(l) => l.get_texture(name).or_else(|| self.builtin.get_texture(name)),
            None => self.builtin.get_texture(name),
        }
    }

    /// Same chain for configuration values. Note the user skin's own
    /// misses already fall back internally where lazer's do (combo
    /// colours default inside `SkinConfiguration`).
    fn get_config(&self, lookup: SkinLookup) -> Option<super::lookup::SkinValue> {
        match &self.legacy {
            Some(l) => l.get_config(lookup.clone()).or_else(|| self.builtin.get_config(lookup)),
            None => self.builtin.get_config(lookup),
        }
    }

    fn get_sample(&self, name: &str) -> Option<std::path::PathBuf> {
        match &self.legacy {
            Some(l) => l.get_sample(name).or_else(|| self.builtin.get_sample(name)),
            None => self.builtin.get_sample(name),
        }
    }
}

impl SkinTextureSource for ResolvedSkin {
    fn texture_images(&self) -> Vec<(String, crate::draw::Image)> {
        // User skin first; builtin sprites whose names the user skin
        // already provides are DROPPED. `assign_regions` keys by name, so
        // a duplicate would make the later (builtin) handle overwrite the
        // user's inside the skin's texture table - a user `approachcircle`
        // (140px) would silently become the builtin 256px one and render
        // oversized. Dropping matches the lookup chain: `get_texture`
        // never reaches the builtin for a name the legacy skin serves.
        let mut images = self.legacy.as_ref().map(|l| l.texture_images()).unwrap_or_default();
        let taken: std::collections::HashSet<String> = images.iter().map(|(n, _)| n.clone()).collect();
        images.extend(
            self.builtin
                .texture_images()
                .into_iter()
                .filter(|(n, _)| !taken.contains(n)),
        );
        images
    }

    fn assign_regions(&mut self, regions: &[(String, SkinTexture)]) {
        if let Some(l) = &mut self.legacy {
            l.assign_regions(regions);
        }
        self.builtin.assign_regions(regions);
    }
}

/// `SkinManager`'s current-skin resolution for this renderer: load a
/// user skin directory when given (the unpacked `.osk` content, or the
/// game's `Skins/<name>` folder), else the built-in default.
pub fn load_skin(path: Option<&Path>) -> Result<ResolvedSkin, String> {
    let builtin = ArgonSkin::new();
    match path {
        Some(p) => {
            let legacy = LegacySkin::from_directory(p)?;
            eprintln!(
                "skin: \"{}\" (legacy v{}, {} texture files) - missing elements fall back to argon",
                legacy.name(),
                legacy.configuration().effective_legacy_version(),
                legacy.texture_count()
            );
            Ok(ResolvedSkin { legacy: Some(legacy), builtin })
        }
        None => Ok(ResolvedSkin { legacy: None, builtin }),
    }
}
