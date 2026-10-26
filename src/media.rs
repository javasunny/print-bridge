use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaperSize {
    A3,
    #[default]
    A4,
    A5,
    A6,
    Letter,
    Legal,
    Tabloid,
}

pub const PAPER_SIZES: [PaperSize; 7] = [
    PaperSize::A3,
    PaperSize::A4,
    PaperSize::A5,
    PaperSize::A6,
    PaperSize::Letter,
    PaperSize::Legal,
    PaperSize::Tabloid,
];

impl PaperSize {
    pub fn name(self) -> &'static str {
        match self {
            Self::A3 => "A3",
            Self::A4 => "A4",
            Self::A5 => "A5",
            Self::A6 => "A6",
            Self::Letter => "Letter",
            Self::Legal => "Legal",
            Self::Tabloid => "Tabloid",
        }
    }

    pub fn keyword(self) -> &'static str {
        match self {
            Self::A3 => "iso_a3_297x420mm",
            Self::A4 => "iso_a4_210x297mm",
            Self::A5 => "iso_a5_148x210mm",
            Self::A6 => "iso_a6_105x148mm",
            Self::Letter => "na_letter_8.5x11in",
            Self::Legal => "na_legal_8.5x14in",
            Self::Tabloid => "na_ledger_11x17in",
        }
    }

    pub fn dimensions(self) -> (i32, i32) {
        match self {
            Self::A3 => (29700, 42000),
            Self::A4 => (21000, 29700),
            Self::A5 => (14800, 21000),
            Self::A6 => (10500, 14800),
            Self::Letter => (21590, 27940),
            Self::Legal => (21590, 35560),
            Self::Tabloid => (27940, 43180),
        }
    }

    pub fn windows_paper_kind(self) -> u16 {
        match self {
            Self::A3 => 8,
            Self::A4 => 9,
            Self::A5 => 11,
            Self::A6 => 70,
            Self::Letter => 1,
            Self::Legal => 5,
            Self::Tabloid => 3,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        PAPER_SIZES
            .into_iter()
            .find(|size| value == size.keyword() || value == size.name())
    }
}

pub(crate) fn default_paper_sizes() -> Vec<PaperSize> {
    vec![PaperSize::A4]
}
