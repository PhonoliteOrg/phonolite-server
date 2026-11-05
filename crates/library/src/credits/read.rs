//! One snapshot's lazily opened credit and metadata tables. Request owners can
//! share decoded rows without retaining catalogue state beyond their read.
use std::collections::HashMap;

use common::{Album, Artist, ArtistCredits, Track};
use redb::{ReadOnlyTable, ReadTransaction, ReadableTable, TableDefinition, TableError};
use serde::de::DeserializeOwned;

use super::{presented_credits, source_credits, DISPLAY_CREDIT_RESOLUTIONS};
use crate::storage::{
    decode_value, ALBUMS_TABLE, ALBUM_CREDITS_TABLE, ARTISTS_TABLE, TRACKS_TABLE,
    TRACK_CREDITS_TABLE,
};
use crate::LibraryError;

struct LazyTable<'txn, 'db> {
    read: &'txn ReadTransaction<'db>,
    definition: TableDefinition<'static, &'static str, &'static [u8]>,
    optional: bool,
    opened: bool,
    table: Option<ReadOnlyTable<'txn, &'static str, &'static [u8]>>,
    #[cfg(test)]
    opens: usize,
    #[cfg(test)]
    decodes: usize,
}

impl<'txn, 'db> LazyTable<'txn, 'db> {
    fn new(
        read: &'txn ReadTransaction<'db>,
        definition: TableDefinition<'static, &'static str, &'static [u8]>,
        optional: bool,
    ) -> Self {
        Self {
            read,
            definition,
            optional,
            opened: false,
            table: None,
            #[cfg(test)]
            opens: 0,
            #[cfg(test)]
            decodes: 0,
        }
    }

    fn open(&mut self) -> Result<(), LibraryError> {
        if !self.opened {
            #[cfg(test)]
            {
                self.opens += 1;
            }
            self.table = match self.read.open_table(self.definition) {
                Ok(table) => Some(table),
                Err(TableError::TableDoesNotExist(_)) if self.optional => None,
                Err(error) => return Err(error.into()),
            };
            self.opened = true;
        }
        Ok(())
    }

    fn get<T: DeserializeOwned>(&mut self, id: &str) -> Result<Option<T>, LibraryError> {
        self.open()?;
        let Some(table) = &self.table else {
            return Ok(None);
        };
        let Some(value) = table.get(id)? else {
            return Ok(None);
        };
        #[cfg(test)]
        {
            self.decodes += 1;
        }
        decode_value(value.value()).map(Some)
    }
}

pub(crate) struct ArtistMetadata {
    pub(crate) name: String,
    pub(crate) genres: Vec<String>,
}

pub(crate) struct AlbumMetadata {
    pub(crate) title: String,
    pub(crate) genres: Vec<String>,
    artist_id: String,
    artist_ids: Vec<String>,
    artist_names: Vec<String>,
}

impl From<Artist> for ArtistMetadata {
    fn from(artist: Artist) -> Self {
        Self {
            name: artist.name,
            genres: artist.genres,
        }
    }
}

impl From<Album> for AlbumMetadata {
    fn from(album: Album) -> Self {
        let artist_ids = album.all_artist_ids().to_vec();
        Self {
            title: album.title,
            genres: album.genres,
            artist_id: album.artist_id,
            artist_ids,
            artist_names: album.artist_names,
        }
    }
}

pub(crate) struct CreditReader<'txn, 'db> {
    artist_rows: LazyTable<'txn, 'db>,
    album_rows: LazyTable<'txn, 'db>,
    track_rows: LazyTable<'txn, 'db>,
    album_credit_rows: LazyTable<'txn, 'db>,
    track_credit_rows: LazyTable<'txn, 'db>,
    resolution_rows: LazyTable<'txn, 'db>,
    artists: HashMap<String, Option<ArtistMetadata>>,
    albums: HashMap<String, Option<AlbumMetadata>>,
    album_credits: HashMap<String, ArtistCredits>,
    track_credits: HashMap<String, ArtistCredits>,
    resolutions: HashMap<String, Option<ArtistCredits>>,
}

impl<'txn, 'db> CreditReader<'txn, 'db> {
    pub(crate) fn new(read: &'txn ReadTransaction<'db>) -> Self {
        Self {
            artist_rows: LazyTable::new(read, ARTISTS_TABLE, false),
            album_rows: LazyTable::new(read, ALBUMS_TABLE, false),
            track_rows: LazyTable::new(read, TRACKS_TABLE, false),
            album_credit_rows: LazyTable::new(read, ALBUM_CREDITS_TABLE, true),
            track_credit_rows: LazyTable::new(read, TRACK_CREDITS_TABLE, true),
            resolution_rows: LazyTable::new(read, DISPLAY_CREDIT_RESOLUTIONS, true),
            artists: HashMap::new(),
            albums: HashMap::new(),
            album_credits: HashMap::new(),
            track_credits: HashMap::new(),
            resolutions: HashMap::new(),
        }
    }

    pub(crate) fn require_artists(&mut self) -> Result<(), LibraryError> {
        self.artist_rows.open()
    }

    pub(crate) fn require_albums(&mut self) -> Result<(), LibraryError> {
        self.album_rows.open()
    }

    pub(crate) fn artist(&mut self, id: &str) -> Result<Option<&ArtistMetadata>, LibraryError> {
        if !self.artists.contains_key(id) {
            let artist = self.artist_rows.get::<Artist>(id)?;
            self.artists.insert(id.to_owned(), artist.map(Into::into));
        }
        Ok(self.artists.get(id).and_then(Option::as_ref))
    }

    pub(crate) fn album(&mut self, id: &str) -> Result<Option<&AlbumMetadata>, LibraryError> {
        if !self.albums.contains_key(id) {
            let album = self.album_rows.get::<Album>(id)?;
            self.albums.insert(id.to_owned(), album.map(Into::into));
        }
        Ok(self.albums.get(id).and_then(Option::as_ref))
    }

    fn resolution(&mut self, key: &str) -> Result<Option<ArtistCredits>, LibraryError> {
        if !self.resolutions.contains_key(key) {
            self.resolutions
                .insert(key.to_owned(), self.resolution_rows.get(key)?);
        }
        Ok(self.resolutions.get(key).cloned().flatten())
    }

    pub(crate) fn album_credits(&mut self, id: &str) -> Result<ArtistCredits, LibraryError> {
        if let Some(credits) = self.album_credits.get(id) {
            return Ok(credits.clone());
        }
        let credits = self.load_album_credits(id)?;
        self.album_credits.insert(id.to_owned(), credits.clone());
        Ok(credits)
    }

    pub(crate) fn load_album_credits(&mut self, id: &str) -> Result<ArtistCredits, LibraryError> {
        let credits = if let Some(credits) = self.album_credit_rows.get(id)? {
            presented_credits(credits, |key| self.resolution(key))?
        } else if let Some(album) = self.album(id)? {
            let artist_id = album.artist_id.clone();
            let artist_ids = album.artist_ids.clone();
            let mut names = album.artist_names.clone();
            if names.is_empty() {
                if let Some(artist) = self.artist(&artist_id)? {
                    names.push(artist.name.clone());
                }
            }
            let mut credits = source_credits(&names.join(", "), &names, "album_artist", "legacy");
            for (credit, artist_id) in credits.credits.iter_mut().zip(artist_ids) {
                credit.artist_id = artist_id;
            }
            credits
        } else {
            ArtistCredits::default()
        };
        Ok(credits)
    }

    pub(crate) fn track_credits(&mut self, id: &str) -> Result<ArtistCredits, LibraryError> {
        if let Some(credits) = self.track_credits.get(id) {
            return Ok(credits.clone());
        }
        let credits = self.load_track_credits(id)?;
        self.track_credits.insert(id.to_owned(), credits.clone());
        Ok(credits)
    }

    pub(crate) fn load_track_credits(&mut self, id: &str) -> Result<ArtistCredits, LibraryError> {
        let credits = if let Some(credits) = self.track_credit_rows.get(id)? {
            presented_credits(credits, |key| self.resolution(key))?
        } else if let Some(track) = self.track_rows.get::<Track>(id)? {
            let name = self
                .artist(&track.artist_id)?
                .map(|artist| artist.name.clone())
                .unwrap_or_default();
            let mut credits =
                source_credits(&name, std::slice::from_ref(&name), "track_artist", "legacy");
            if let Some(credit) = credits.credits.first_mut() {
                credit.artist_id = track.artist_id;
            }
            credits
        } else {
            ArtistCredits::default()
        };
        Ok(credits)
    }

    #[cfg(test)]
    pub(crate) fn work(&self) -> (usize, usize) {
        let tables = [
            &self.artist_rows,
            &self.album_rows,
            &self.track_rows,
            &self.album_credit_rows,
            &self.track_credit_rows,
            &self.resolution_rows,
        ];
        (
            tables.iter().map(|table| table.opens).sum(),
            tables.iter().map(|table| table.decodes).sum(),
        )
    }
}
