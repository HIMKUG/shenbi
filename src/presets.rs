use crate::imaging::{Gravity, Operation};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    pub size: &'static str,
}

pub const PRESETS: [Preset; 9] = [
    Preset { id: "profile_pic", label: "Profile picture", detail: "200×200 · circle", size: "square" },
    Preset { id: "post_image", label: "Post image", detail: "1200×675 · 16:9", size: "landscape" },
    Preset { id: "hero_photo", label: "Hero photo", detail: "1080×1920 · 9:16", size: "portrait" },
    Preset { id: "avatar", label: "Avatar", detail: "80×80 · circle", size: "square" },
    Preset { id: "scene", label: "Scene", detail: "1200×675 · 16:9", size: "landscape" },
    Preset { id: "avery_8293", label: "Avery 8293", detail: "450×450 · 1.5″ circle", size: "square" },
    Preset { id: "avery_5160", label: "Avery 5160", detail: "788×300 · address", size: "landscape" },
    Preset { id: "avery_22830", label: "Avery 22830", detail: "750×750 · 2.5″ circle", size: "square" },
    Preset { id: "avery_8164", label: "Avery 8164", detail: "1000×1200 · shipping", size: "portrait" },
];

pub fn find(id: &str) -> Option<Preset> {
    PRESETS.iter().copied().find(|p| p.id == id)
}

/// Post-processing chain applied after generation for each preset.
pub fn operations(preset: &str) -> Vec<Operation> {
    let circle = |w, h| {
        vec![
            Operation::AspectCrop { ratio: "1:1".into(), gravity: Some(Gravity::Center) },
            Operation::CircleMask,
            Operation::Resize { width: Some(w), height: Some(h), fit: None, without_enlargement: false },
        ]
    };
    match preset {
        "profile_pic" => circle(200, 200),
        "avatar" => circle(80, 80),
        "avery_8293" => circle(450, 450),
        "avery_22830" => circle(750, 750),
        "post_image" | "scene" => vec![
            Operation::AspectCrop { ratio: "16:9".into(), gravity: Some(Gravity::Center) },
            Operation::Resize { width: Some(1200), height: Some(675), fit: None, without_enlargement: false },
        ],
        "hero_photo" => vec![
            Operation::AspectCrop { ratio: "9:16".into(), gravity: Some(Gravity::Center) },
            Operation::Resize { width: Some(1080), height: Some(1920), fit: None, without_enlargement: true },
        ],
        "avery_5160" => vec![Operation::Resize {
            width: Some(788),
            height: Some(300),
            fit: Some(crate::imaging::Fit::Cover),
            without_enlargement: false,
        }],
        "avery_8164" => vec![
            Operation::AspectCrop { ratio: "3:4".into(), gravity: Some(Gravity::Center) },
            Operation::Resize { width: Some(1000), height: Some(1200), fit: None, without_enlargement: false },
        ],
        _ => Vec::new(),
    }
}
