use std::path::{Path, PathBuf};

/// Writes a minimal Chinook-shaped `SQLite` DB into `dir` and returns its path.
/// Table/column names match real Chinook so tests assert on familiar values.
pub fn write_chinook_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("chinook.sqlite");
    let conn = rusqlite::Connection::open(&path).expect("open fixture");
    conn.execute_batch(
        "CREATE TABLE Artist (ArtistId INTEGER PRIMARY KEY, Name NVARCHAR(120));
         CREATE TABLE Album (AlbumId INTEGER PRIMARY KEY, Title NVARCHAR(160) NOT NULL,
             ArtistId INTEGER NOT NULL,
             FOREIGN KEY (ArtistId) REFERENCES Artist (ArtistId));
         CREATE INDEX IFK_AlbumArtistId ON Album (ArtistId);
         INSERT INTO Artist VALUES (1,'AC/DC'),(2,'Accept');
         INSERT INTO Album VALUES
             (1,'For Those About To Rock We Salute You',1),
             (2,'Balls to the Wall',2),
             (3,'Restless and Wild',2);
         -- A wide table for the uncapped-sample test: 25 rows * ~4 KiB > 64 KiB at LIMIT 20.
         CREATE TABLE Wide (id INTEGER PRIMARY KEY, payload TEXT NOT NULL);",
    )
    .expect("seed schema");
    {
        let mut stmt = conn
            .prepare("INSERT INTO Wide (id, payload) VALUES (?1, ?2)")
            .unwrap();
        let pad = "x".repeat(4000);
        for i in 1..=25 {
            stmt.execute(rusqlite::params![i, pad]).unwrap();
        }
    }
    drop(conn);
    path
}
