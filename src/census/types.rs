//! Path segments of `/geocoder/{returntype}/{searchtype}`.

use serde::{Deserialize, Serialize};

use crate::cache::key::SearchType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReturnType {
    Locations,
    Geographies,
}

impl ReturnType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "locations" => Some(Self::Locations),
            "geographies" => Some(Self::Geographies),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Locations => "locations",
            Self::Geographies => "geographies",
        }
    }
}

/// The `{searchtype}` path segment (`onelineaddress`, not the key's `oneline`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPath {
    OnelineAddress,
    Address,
    AddressPR,
    Coordinates,
}

impl SearchPath {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "onelineaddress" => Some(Self::OnelineAddress),
            "address" => Some(Self::Address),
            "addressPR" => Some(Self::AddressPR),
            "coordinates" => Some(Self::Coordinates),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnelineAddress => "onelineaddress",
            Self::Address => "address",
            Self::AddressPR => "addressPR",
            Self::Coordinates => "coordinates",
        }
    }

    pub fn search_type(self) -> SearchType {
        match self {
            Self::OnelineAddress => SearchType::Oneline,
            Self::Address => SearchType::Address,
            Self::AddressPR => SearchType::AddressPR,
            Self::Coordinates => SearchType::Coordinates,
        }
    }
}
