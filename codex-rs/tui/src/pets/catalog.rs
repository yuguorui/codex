//! Built-in pet catalog ported from the Codex App avatar catalog.

pub(super) const DEFAULT_FRAME_WIDTH: u32 = 192;
pub(super) const DEFAULT_FRAME_HEIGHT: u32 = 208;
pub(super) const DEFAULT_FRAME_COLUMNS: u32 = 8;
pub(super) const DEFAULT_FRAME_ROWS: u32 = 9;
pub(super) const SPRITESHEET_WIDTH: u32 = DEFAULT_FRAME_WIDTH * DEFAULT_FRAME_COLUMNS;
pub(super) const SPRITESHEET_HEIGHT: u32 = DEFAULT_FRAME_HEIGHT * DEFAULT_FRAME_ROWS;

pub(crate) const BONGO_CAT_PET_ID: &str = "bongo-cat";

const BONGO_CAT_SPRITESHEET: &[u8] = include_bytes!("../../assets/pets/bongo-cat-spritesheet.png");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BuiltinPetAsset {
    Cdn,
    Bundled(&'static [u8]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BuiltinPetBehavior {
    Standard,
    Typing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BuiltinPet {
    pub(super) id: &'static str,
    pub(super) display_name: &'static str,
    pub(super) description: &'static str,
    pub(super) spritesheet_file: &'static str,
    pub(super) frame_width: u32,
    pub(super) frame_height: u32,
    pub(super) columns: u32,
    pub(super) rows: u32,
    pub(super) asset: BuiltinPetAsset,
    pub(super) behavior: BuiltinPetBehavior,
}

impl BuiltinPet {
    const fn cdn(
        id: &'static str,
        display_name: &'static str,
        description: &'static str,
        spritesheet_file: &'static str,
    ) -> Self {
        Self {
            id,
            display_name,
            description,
            spritesheet_file,
            frame_width: DEFAULT_FRAME_WIDTH,
            frame_height: DEFAULT_FRAME_HEIGHT,
            columns: DEFAULT_FRAME_COLUMNS,
            rows: DEFAULT_FRAME_ROWS,
            asset: BuiltinPetAsset::Cdn,
            behavior: BuiltinPetBehavior::Standard,
        }
    }

    pub(super) fn spritesheet_width(self) -> u32 {
        self.frame_width * self.columns
    }

    pub(super) fn spritesheet_height(self) -> u32 {
        self.frame_height * self.rows
    }

    pub(super) fn frame_count(self) -> usize {
        (self.columns * self.rows) as usize
    }
}

pub(super) const BUILTIN_PETS: &[BuiltinPet] = &[
    BuiltinPet::cdn(
        "codex",
        "Codex",
        "The original Codex companion",
        "codex-spritesheet-v4.webp",
    ),
    BuiltinPet::cdn(
        "dewey",
        "Dewey",
        "A tidy duck for calm workspace days",
        "dewey-spritesheet-v4.webp",
    ),
    BuiltinPet::cdn(
        "fireball",
        "Fireball",
        "Hot path energy for fast iteration",
        "fireball-spritesheet-v4.webp",
    ),
    BuiltinPet::cdn(
        "rocky",
        "Rocky",
        "A steady rock when the diff gets large",
        "rocky-spritesheet-v4.webp",
    ),
    BuiltinPet::cdn(
        "seedy",
        "Seedy",
        "Small green shoots for new ideas",
        "seedy-spritesheet-v4.webp",
    ),
    BuiltinPet::cdn(
        "stacky",
        "Stacky",
        "A balanced stack for deep work",
        "stacky-spritesheet-v4.webp",
    ),
    BuiltinPet::cdn(
        "bsod",
        "BSOD",
        "A tiny blue-screen gremlin",
        "bsod-spritesheet-v4.webp",
    ),
    BuiltinPet::cdn(
        "null-signal",
        "Null Signal",
        "Quiet signal from the void",
        "null-signal-spritesheet-v4.webp",
    ),
    BuiltinPet {
        id: BONGO_CAT_PET_ID,
        display_name: "Bongo Cat",
        description: "A terminal cat that types along with you",
        spritesheet_file: "bongo-cat-spritesheet-v4.png",
        frame_width: 200,
        frame_height: 126,
        columns: 3,
        rows: 1,
        asset: BuiltinPetAsset::Bundled(BONGO_CAT_SPRITESHEET),
        behavior: BuiltinPetBehavior::Typing,
    },
];

pub(super) fn builtin_pet(id: &str) -> Option<BuiltinPet> {
    BUILTIN_PETS.iter().copied().find(|pet| pet.id == id)
}

#[cfg(test)]
pub(super) fn write_test_spritesheet(path: &std::path::Path) {
    let image = image::RgbaImage::new(SPRITESHEET_WIDTH, SPRITESHEET_HEIGHT);
    image.save(path).unwrap();
}

#[cfg(test)]
pub(super) fn write_test_builtin_spritesheet(path: &std::path::Path, pet: BuiltinPet) {
    let image = image::RgbaImage::new(pet.spritesheet_width(), pet.spritesheet_height());
    image.save(path).unwrap();
}
