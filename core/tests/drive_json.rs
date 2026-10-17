//! Real-world Google Drive v3 JSON must deserialize into our types.
//! Drive uses camelCase keys and returns `size` as a *string*.

use clouddrive_core::types::{Change, DriveFile};

const FILE_JSON: &str = r#"{
  "id": "1AbCdEfG",
  "name": "IMG_0001.jpg",
  "mimeType": "image/jpeg",
  "parents": ["0AExampleRoot"],
  "size": "2345678",
  "modifiedTime": "2024-05-01T10:20:30.123Z",
  "md5Checksum": "d41d8cd98f00b204e9800998ecf8427e",
  "trashed": false,
  "thumbnailLink": "https://lh3.googleusercontent.com/abc=s220"
}"#;

#[test]
fn drive_file_parses_real_drive_json() {
    let f: DriveFile = serde_json::from_str(FILE_JSON)
        .expect("a normal Drive file entry must deserialize");
    assert_eq!(f.mime_type, "image/jpeg");
    assert_eq!(f.size, Some(2_345_678));
    assert_eq!(f.md5_checksum.as_deref(), Some("d41d8cd98f00b204e9800998ecf8427e"));
}

#[test]
fn change_parses_real_drive_json() {
    let json = format!(r#"{{"kind":"drive#change","fileId":"1AbCdEfG","removed":false,"file":{FILE_JSON}}}"#);
    let c: Change = serde_json::from_str(&json).expect("a change entry must deserialize");
    assert_eq!(c.file_id, "1AbCdEfG");
}
