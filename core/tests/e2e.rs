//! End-to-end: the real engine against a fake Google.
//! Drives the engine exactly like the GUI does.

mod mock_google;
use clouddrive_core::*;
use mock_google::*;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

static CFG_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

struct Harness {
    mock: Mock,
    engine: Arc<CoreEngine>,
    _dirs: tempfile::TempDir,
}

/// Simulate the browser: read redirect_uri/state out of the consent URL and "approve".
fn approve(url: &str, code: &str) {
    let u = url::Url::parse(url).unwrap();
    let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
    assert_eq!(q["prompt"], "select_account consent", "must always show the account chooser");
    let target = format!("{}/?code={code}&state={}", q["redirect_uri"], q["state"]);
    tokio::spawn(async move {
        let _ = reqwest::get(&target).await;
    });
}

async fn wait_for<F: FnMut() -> bool>(what: &str, mut f: F) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn setup() -> Harness {
    // Keep config.toml writes away from the real home directory.
    let cfg = CFG_DIR.get_or_init(|| tempfile::tempdir().unwrap());
    std::env::set_var("CLOUDDRIVE_CONFIG_DIR", cfg.path());
    std::env::set_var("CLOUDY_NO_BROWSER", "1");

    let mock = Mock::start().await;
    mock.add_account("a@x.com", "ROOT_A");
    mock.add_account("b@x.com", "ROOT_B");
    // Account A
    mock.add_file("a@x.com", mfolder("A_photos", "Photos", "ROOT_A"));
    mock.add_file("a@x.com", mfile("A_img1", "beach.jpg", "A_photos", b"0123456789ABCDEF"));
    mock.add_file("a@x.com", mfile("A_img2", "dup.jpg", "A_photos", b"aaaa"));
    mock.add_file("a@x.com", mfolder("A_docs", "Docs", "ROOT_A"));
    mock.add_file("a@x.com", mfile("A_note", "note.txt", "A_docs", b"hello from A"));
    mock.add_file("a@x.com", mfile("A_top", "top.txt", "ROOT_A", b"top"));
    // Account B: also has "Photos", plus a file shared *with* B (must be ignored)
    mock.add_file("b@x.com", mfolder("B_photos", "Photos", "ROOT_B"));
    mock.add_file("b@x.com", mfile("B_img1", "mountain.jpg", "B_photos", b"MOUNTAIN"));
    mock.add_file("b@x.com", mfile("B_img2", "dup.jpg", "B_photos", b"bbbb"));
    let mut shared = mfile("B_shared", "not-mine.txt", "SOMEONE_ELSES", b"x");
    shared.owned = false;
    mock.add_file("b@x.com", shared);

    let dirs = tempfile::tempdir().unwrap();
    let store = Arc::new(SecretStore::file_backed(dirs.path().join("cfg")));
    let engine = Arc::new(
        CoreEngine::with_parts(
            AppConfig::default(),
            Endpoints::local(&mock.addr),
            store,
            clouddrive_core::engine::EngineDirs { data: dirs.path().join("data"), cache: dirs.path().join("cache") },
        )
        .unwrap(),
    );
    engine.set_client_credentials("123-test.apps.googleusercontent.com", "secret").unwrap();
    Harness { mock, engine, _dirs: dirs }
}

async fn add(h: &Harness, email: &str) -> AddAccountResult {
    let code = format!("code-{email}");
    h.engine.add_account(None, |url| approve(url, &code)).await.unwrap()
}

fn pool_names(e: &CoreEngine, id: &str) -> Vec<String> {
    e.list_children(id).into_iter().map(|n| n.display_name).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_user_journey() {
    let h = setup().await;
    let e = &h.engine;

    // ---- 1. add two accounts (the bug: the 2nd one never worked) -----------------
    let a = add(&h, "a@x.com").await;
    assert_eq!(a.email, "a@x.com");
    assert!(!a.already_linked);
    let b = add(&h, "b@x.com").await;
    assert_eq!(b.email, "b@x.com");
    assert_eq!(e.list_accounts(), vec!["a@x.com", "b@x.com"]);

    // ---- 2. existing data actually loads (the other bug) -------------------------
    // Mid-index, files whose folder has not arrived yet show up at the top level, so the
    // *count* alone can match too early: wait until the folders themselves are there.
    wait_for("both accounts indexed", || {
        let has = |a: &str, name: &str| pool_names(e, a).contains(&name.to_string());
        let n = |a: &str| e.list_children(a).len();
        has("a@x.com", "Photos") && has("a@x.com", "Docs") && n("a@x.com") == 3 && has("b@x.com", "Photos") && n("b@x.com") == 1
    })
    .await;
    let top = pool_names(e, "pool");
    assert!(top.contains(&"Photos".to_string()) && top.contains(&"Docs".to_string()) && top.contains(&"top.txt".to_string()), "{top:?}");

    // pooled folder = union of both accounts' "Photos"
    let photos = e.lookup_child("pool", "Photos").unwrap();
    assert_eq!(photos.sources.len(), 2, "same-named folders must merge");
    let mut inside = pool_names(e, &photos.id);
    inside.sort();
    assert_eq!(inside.len(), 4, "{inside:?}");
    assert!(inside.contains(&"beach.jpg".to_string()) && inside.contains(&"mountain.jpg".to_string()));
    assert_eq!(inside.iter().filter(|n| n.starts_with("dup")).count(), 2, "both dup.jpg kept, disambiguated");

    // files shared *with* the user are not part of their storage
    assert!(e.search("not-mine").unwrap().is_empty());
    // sizes arrived as strings from Drive and were parsed
    let beach = e.search("beach").unwrap().pop().unwrap();
    assert_eq!(beach.size, 16);

    // ---- 3. account list shows quota + sync state ---------------------------------
    let sums = e.account_summaries(true).await;
    assert_eq!(sums.len(), 2);
    assert_eq!(sums[0].quota.as_ref().unwrap().limit, 16_106_127_360);
    assert_eq!(sums[0].display_name.as_deref(), Some("A"));
    assert_eq!(sums[0].files_indexed, 6);

    // re-adding an existing account is reported, not silently duplicated
    let again = add(&h, "a@x.com").await;
    assert!(again.already_linked);
    assert_eq!(e.list_accounts().len(), 2);

    // ---- 4. content: ranged read + thumbnails -------------------------------------
    let m = e.fetch_media(&beach.id, Some((2, Some(5)))).await.unwrap();
    assert_eq!(&m.bytes[..], b"2345");
    assert_eq!((m.start, m.total), (2, 16));
    let whole = e.read_content(&beach.id, 0, 100).await.unwrap();
    assert_eq!(&whole[..], b"0123456789ABCDEF");

    let (thumb, mime) = e.thumbnail(&beach.id, 256).await.unwrap();
    assert_eq!(mime, "image/jpeg");
    assert_eq!(&thumb[..], JPEG);
    e.thumbnail(&beach.id, 256).await.unwrap();
    assert_eq!(h.mock.with(|s| s.thumb_hits), 1, "second thumbnail served from disk cache");
    assert_eq!(e.list_media_files().unwrap().len(), 4, "gallery lists the 4 photos");

    // ---- 5. incremental sync picks up remote changes (paginated change feed) -------
    h.mock.add_file("a@x.com", mfile("A_new1", "new1.txt", "ROOT_A", b"1"));
    h.mock.add_file("a@x.com", mfile("A_new2", "new2.txt", "ROOT_A", b"2"));
    h.mock.add_file("a@x.com", mfile("A_new3", "new3.txt", "ROOT_A", b"3"));
    h.mock.with(|s| s.accounts.get_mut("a@x.com").unwrap().remove("A_top"));
    e.force_sync("a@x.com").await.unwrap();
    wait_for("changes applied", || {
        let n = pool_names(e, "pool");
        n.contains(&"new3.txt".to_string()) && !n.contains(&"top.txt".to_string())
    })
    .await;
    assert!(pool_names(e, "pool").contains(&"new1.txt".to_string()));

    // ---- 6. access token expiry is handled transparently ---------------------------
    h.mock.with(|s| s.accounts.get_mut("b@x.com").unwrap().token_gen += 5); // old token now invalid
    let q = e.get_all_quotas().await.unwrap();
    assert!(q.contains_key("b@x.com"), "client refreshed the token and retried");

    // ---- 7. upload into the pool; chunks reassemble; file appears ------------------
    let tmp = tempfile::tempdir().unwrap();
    let big: Vec<u8> = (0..(5 * 1024 * 1024 + 123)).map(|i| (i % 251) as u8).collect(); // > 1 chunk
    let path = tmp.path().join("movie.txt");
    std::fs::write(&path, &big).unwrap();
    let ids = e.upload_paths(vec![path.clone()], "pool").await.unwrap();
    assert_eq!(ids.len(), 1);
    wait_for("upload completed", || {
        e.get_transfers().iter().any(|t| t.id == ids[0] && t.status == TransferStatus::Completed)
    })
    .await;
    wait_for("uploaded file visible", || pool_names(e, "pool").contains(&"movie.txt".to_string())).await;
    let uploaded = h.mock.with(|s| {
        s.accounts.values().flat_map(|a| a.files.iter()).find(|f| f.name == "movie.txt").map(|f| f.content.clone())
    });
    assert_eq!(uploaded.as_deref(), Some(&big[..]), "bytes identical after chunked upload");

    // ---- 8. upload a folder into a pooled folder: lazy folder creation -------------
    let src = tmp.path().join("Trip");
    std::fs::create_dir_all(src.join("day1")).unwrap();
    std::fs::write(src.join("day1/a.txt"), "aaa").unwrap();
    // pin to B, which has no "Docs" folder → it must be created there on demand
    let mut cfg = e.get_config();
    cfg.upload_policy = UploadPolicy::PinnedAccount("b@x.com".into());
    e.update_config(cfg).unwrap();
    let docs = e.lookup_child("pool", "Docs").unwrap();
    let ids = e.upload_paths(vec![src], &docs.id).await.unwrap();
    wait_for("folder upload completed", || {
        e.get_transfers().iter().any(|t| t.id == ids[0] && t.status == TransferStatus::Completed)
    })
    .await;
    wait_for("nested upload visible", || {
        e.lookup_child("pool", "Docs")
            .and_then(|d| e.lookup_child(&d.id, "Trip"))
            .and_then(|t| e.lookup_child(&t.id, "day1"))
            .and_then(|d| e.lookup_child(&d.id, "a.txt"))
            .is_some()
    })
    .await;
    let docs = e.lookup_child("pool", "Docs").unwrap();
    assert_eq!(docs.sources.len(), 2, "Docs now exists in both accounts and is shown as one folder");

    // ---- 9. trash removes from the pool ------------------------------------------
    let nid = e.lookup_child("pool", "new1.txt").unwrap().id;
    e.trash_file(&nid).await.unwrap();
    assert!(e.lookup_child("pool", "new1.txt").is_none());
    let trash = e.list_trash().await.unwrap();
    assert!(trash.iter().any(|t| t.name == "new1.txt"), "trashed items are listed (they still use quota)");

    // ---- 10. revoked login → clear, recoverable state -----------------------------
    h.mock.with(|s| {
        let a = s.accounts.get_mut("b@x.com").unwrap();
        a.revoked = true;
        a.token_gen += 1;
    });
    e.force_sync("b@x.com").await.unwrap();
    wait_for("needs-reauth surfaced", || e.get_sync_status("b@x.com") == SyncStatus::NeedsReauth).await;
    let sums = e.account_summaries(true).await;
    let b = sums.iter().find(|s| s.email == "b@x.com").unwrap();
    assert_eq!(b.sync, SyncStatus::NeedsReauth);
    assert!(sums.iter().find(|s| s.email == "a@x.com").unwrap().sync != SyncStatus::NeedsReauth, "other accounts unaffected");
    // …and previously indexed data stays browsable meanwhile
    assert!(e.lookup_child("pool", "Photos").is_some());

    // reconnect with the right account works; the wrong one is rejected
    h.mock.with(|s| s.accounts.get_mut("b@x.com").unwrap().revoked = false);
    let wrong = e.reconnect_account("b@x.com", |url| approve(url, "code-a@x.com")).await;
    assert!(matches!(wrong, Err(CloudError::Auth(AuthError::AccountMismatch { .. }))), "{wrong:?}");
    let ok = e.reconnect_account("b@x.com", |url| approve(url, "code-b@x.com")).await.unwrap();
    assert!(ok.already_linked);
    wait_for("b recovered", || e.get_sync_status("b@x.com") != SyncStatus::NeedsReauth).await;

    // ---- 11. removing an account removes its data from the pool --------------------
    e.remove_account("b@x.com").unwrap();
    assert_eq!(e.list_accounts(), vec!["a@x.com"]);
    let photos = e.lookup_child("pool", "Photos").unwrap();
    assert_eq!(photos.all_sources().len(), 1);
    assert!(e.lookup_child(&photos.id, "mountain.jpg").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_survives_restart_and_shows_instantly() {
    let h = setup().await;
    add(&h, "a@x.com").await;
    wait_for("indexed", || pool_names(&h.engine, "a@x.com").contains(&"Photos".to_string())).await;
    h.engine.stop_sync();
    drop(h.engine);

    // New engine, same dirs, *server unreachable*: cached data must still be browsable.
    let dirs = &h._dirs;
    let store = Arc::new(SecretStore::file_backed(dirs.path().join("cfg")));
    let e2 = CoreEngine::with_parts(
        AppConfig::default(),
        Endpoints::local("http://127.0.0.1:1"),
        store,
        clouddrive_core::engine::EngineDirs { data: dirs.path().join("data"), cache: dirs.path().join("cache") },
    )
    .unwrap();
    assert_eq!(e2.list_accounts(), vec!["a@x.com"]);
    assert!(pool_names(&e2, "pool").contains(&"Photos".to_string()), "tree restored from cache with no network");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trashing_a_folder_takes_its_contents_along_and_restoring_brings_them_back() {
    let h = setup().await;
    let e = &h.engine;
    add(&h, "a@x.com").await;
    wait_for("indexed", || pool_names(e, "a@x.com").contains(&"Docs".to_string()) && pool_names(e, "a@x.com").contains(&"Photos".to_string())).await;

    let docs = e.lookup_child("pool", "Docs").unwrap();
    assert!(e.lookup_child(&docs.id, "note.txt").is_some());
    e.trash_file(&docs.id).await.unwrap();
    let top = pool_names(e, "pool");
    assert!(!top.contains(&"Docs".to_string()), "{top:?}");
    assert!(!top.contains(&"note.txt".to_string()), "contents must not spill into the top level: {top:?}");

    e.restore_trashed("a@x.com", "A_docs").await.unwrap();
    wait_for("folder and its contents are back", || {
        e.lookup_child("pool", "Docs").and_then(|d| e.lookup_child(&d.id, "note.txt")).is_some()
    })
    .await;
    assert!(!pool_names(e, "pool").contains(&"note.txt".to_string()), "restored contents belong inside the folder");
}
