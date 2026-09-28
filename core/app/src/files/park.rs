use super::{PortableName, Retained, ScopedDirectory, invalid, leaf};
use serde::{Deserialize, Serialize};
use std::io;

const MAX_PARK_RECEIPT_BYTES: usize = 2048;

/// Domain-owned journals store this before the first namespace effect. These
/// witnesses are equality evidence; recovery still opens exact capabilities
/// beneath the already admitted parent and checks both namespace positions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectoryParkReceipt {
    schema: u8,
    parent: String,
    source: String,
    source_name: String,
    park_name: String,
}

impl DirectoryParkReceipt {
    pub(crate) fn encode(&self) -> String {
        serde_json::to_string(self).expect("bounded receipt fields serialize")
    }

    pub(crate) fn decode(value: &str) -> io::Result<Self> {
        if value.len() > MAX_PARK_RECEIPT_BYTES {
            return Err(invalid("directory park receipt exceeds its bound"));
        }
        let receipt: Self = serde_json::from_str(value)
            .map_err(|_| invalid("directory park receipt is malformed"))?;
        receipt.validate()?;
        Ok(receipt)
    }

    pub(crate) fn source_name(&self) -> &str {
        &self.source_name
    }
    pub(crate) fn park_name(&self) -> &str {
        &self.park_name
    }
    pub(crate) fn source_receipt(&self) -> &str {
        &self.source
    }
    pub(crate) fn matches_source_receipt(&self, expected: &str) -> bool {
        self.source == expected
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema != 1
            || !valid_directory_receipt(&self.parent)
            || !valid_directory_receipt(&self.source)
        {
            return Err(invalid(
                "directory park receipt schema or identity is invalid",
            ));
        }
        let source = PortableName::new_exact(&self.source_name)
            .map_err(|_| invalid("directory park receipt source name is invalid"))?;
        let park = PortableName::new_exact(&self.park_name)
            .map_err(|_| invalid("directory park receipt park name is invalid"))?;
        if source.key() == park.key() {
            return Err(invalid("directory park source and destination must differ"));
        }
        Ok(())
    }
}

fn valid_directory_receipt(value: &str) -> bool {
    value.len() == 142
        && value.starts_with("axial-dir-v1:")
        && value.as_bytes()[77] == b':'
        && value.as_bytes()[13..77]
            .iter()
            .chain(value.as_bytes()[78..].iter())
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// An exact source handle and chosen destination, without any filesystem effect.
#[must_use = "persist the receipt before executing the park plan"]
#[derive(Debug)]
pub(crate) struct DirectoryParkPlan {
    parent: ScopedDirectory,
    source: ScopedDirectory,
    receipt: DirectoryParkReceipt,
}

impl DirectoryParkPlan {
    pub(crate) fn receipt(&self) -> &DirectoryParkReceipt {
        &self.receipt
    }

    pub(crate) fn execute(self) -> Retained<axial_fs::DirectoryParkOutcome> {
        let checked = self
            .parent
            .verify_receipt(&self.receipt.parent)
            .and_then(|_| self.source.verify_receipt(&self.receipt.source));
        let pin = self.source.pin;
        let outcome = match checked {
            Err(error) => axial_fs::DirectoryParkOutcome::NoEffect {
                error,
                directory: self.source.directory,
            },
            Ok(()) => self.source.directory.park_as(leaf(
                &PortableName::new_exact(&self.receipt.park_name)
                    .expect("validated park plan name"),
            )),
        };
        Retained::new(outcome, pin)
    }
}

#[must_use = "a recovered park must be restored, cleaned up, or visibly preserved"]
#[derive(Debug)]
pub(crate) enum RecoveredDirectoryPark {
    Original(ScopedDirectory),
    Parked(Retained<axial_fs::ParkedDirectory>),
    Missing,
    Conflict,
}

impl ScopedDirectory {
    pub(crate) fn plan_park(
        &self,
        source_name: &PortableName,
        park_name: &PortableName,
    ) -> io::Result<DirectoryParkPlan> {
        if source_name.key() == park_name.key() {
            return Err(invalid("directory park source and destination must differ"));
        }
        self.require_absent(park_name)?;
        let source = self.open_directory(source_name)?;
        let receipt = DirectoryParkReceipt {
            schema: 1,
            parent: self.receipt()?,
            source: source.receipt()?,
            source_name: source_name.as_str().to_owned(),
            park_name: park_name.as_str().to_owned(),
        };
        Ok(DirectoryParkPlan {
            parent: self.clone(),
            source,
            receipt,
        })
    }

    pub(crate) fn recover_park(
        &self,
        receipt: &DirectoryParkReceipt,
    ) -> io::Result<RecoveredDirectoryPark> {
        receipt.validate()?;
        if !self.matches_receipt(&receipt.parent)? {
            return Ok(RecoveredDirectoryPark::Conflict);
        }
        let source_name = PortableName::new_exact(&receipt.source_name)
            .map_err(|_| invalid("invalid park source"))?;
        let park_name = PortableName::new_exact(&receipt.park_name)
            .map_err(|_| invalid("invalid park destination"))?;
        let source = self.directory_position(&source_name)?;
        let parked = self.directory_position(&park_name)?;
        match (source, parked) {
            (Position::Directory(source), Position::Missing) => {
                if source.matches_receipt(&receipt.source)? {
                    Ok(RecoveredDirectoryPark::Original(source))
                } else {
                    Ok(RecoveredDirectoryPark::Conflict)
                }
            }
            (Position::Missing, Position::Directory(parked)) => {
                if !parked.matches_receipt(&receipt.source)? {
                    return Ok(RecoveredDirectoryPark::Conflict);
                }
                let revision = parked.revision()?;
                let parked = self.directory.admit_existing_directory_park(
                    &leaf(&source_name),
                    parked.directory,
                    &revision,
                )?;
                Ok(RecoveredDirectoryPark::Parked(Retained::new(
                    parked,
                    self.pin.clone(),
                )))
            }
            (Position::Missing, Position::Missing) => Ok(RecoveredDirectoryPark::Missing),
            _ => Ok(RecoveredDirectoryPark::Conflict),
        }
    }

    fn require_absent(&self, name: &PortableName) -> io::Result<()> {
        let listing = self.entries(axial_fs::MAX_DIRECTORY_LIST_ENTRIES)?;
        if listing.state() != axial_fs::DirectoryListingState::Complete {
            return Err(invalid("directory namespace exceeds its inspection bound"));
        }
        if listing
            .entries()
            .iter()
            .any(|entry| axial_fs::leaf_names_equivalent(entry.name(), leaf(name).as_os_str()))
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "directory park destination exists",
            ));
        }
        Ok(())
    }

    fn directory_position(&self, name: &PortableName) -> io::Result<Position> {
        // Listing also catches files, symlinks and portable aliases. Opening a
        // directory remains no-follow and independently checks its binding.
        let listing = self.entries(axial_fs::MAX_DIRECTORY_LIST_ENTRIES)?;
        if listing.state() != axial_fs::DirectoryListingState::Complete {
            return Err(invalid("directory namespace exceeds its inspection bound"));
        }
        let matching = listing
            .entries()
            .iter()
            .filter(|entry| axial_fs::leaf_names_equivalent(entry.name(), leaf(name).as_os_str()))
            .collect::<Vec<_>>();
        match matching.as_slice() {
            [] => Ok(Position::Missing),
            [entry]
                if entry.utf8_name() == Some(name.as_str())
                    && entry.kind() == axial_fs::EntryKind::Directory =>
            {
                self.open_directory(name).map(Position::Directory)
            }
            _ => Ok(Position::Conflict),
        }
    }
}

enum Position {
    Missing,
    Directory(ScopedDirectory),
    Conflict,
}
