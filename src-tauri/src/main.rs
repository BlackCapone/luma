#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, process::Command};
use tauri::{AppHandle, Manager};

#[derive(Serialize, Deserialize, Clone)]
struct Track {
    id: String,
    title: String,
    artist: String,
    duration: f64,
    url: String,
    #[serde(default)]
    cover: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    local_cover: Option<String>,
    #[serde(default)]
    source: String,
}

// yt-dlp и ffmpeg берутся из PATH или из папки рядом с exe
fn ytdlp() -> Command {
    let mut c = Command::new("yt-dlp");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // без окна консоли
    }
    c
}

fn tracks_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let d = app.path().app_data_dir().map_err(|e| e.to_string())?.join("tracks");
    fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d)
}

fn s(v: &serde_json::Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string().trim_matches('"').to_string())
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{:02X}", b) })
        .collect()
}

fn ytdlp_search(target: String, n: u32, src: &str) -> Result<Vec<Track>, String> {
    let out = ytdlp()
        .args(["--flat-playlist", "--dump-json", "--no-warnings", "--playlist-end"])
        .arg(n.to_string())
        .arg(&target)
        .output()
        .map_err(|e| format!("yt-dlp не найден: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let tracks: Vec<Track> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| {
            let id = s(&v["id"]);
            let url = v["url"].as_str().or(v["webpage_url"].as_str()).map(String::from)
                .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={id}"));
            let artist = v["artist"].as_str().or(v["uploader"].as_str()).or(v["channel"].as_str())
                .unwrap_or("").trim_end_matches(" - Topic").to_string();
            let cover = v["thumbnails"].as_array().and_then(|a| a.last()).and_then(|t| t["url"].as_str())
                .or(v["thumbnail"].as_str()).map(String::from)
                .or_else(|| (src != "sc").then(|| format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg")));
            Track { id: format!("{src}_{id}"), title: s(&v["title"]), artist, duration: v["duration"].as_f64().unwrap_or(0.0),
                url, cover, file: None, local_cover: None, source: src.to_string() }
        })
        .collect();
    if tracks.is_empty() {
        return Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("ничего не найдено").to_string());
    }
    Ok(tracks)
}

// Каталог Spotify (клиентские ключи). Само аудио берётся с YouTube Music.
fn spotify(q: &str, n: u32, id: &str, secret: &str) -> Result<Vec<Track>, String> {
    use base64::Engine;
    if id.is_empty() || secret.is_empty() {
        return Err("нет ключей Spotify".into());
    }
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
    let tok: serde_json::Value = ureq::post("https://accounts.spotify.com/api/token")
        .set("Authorization", &format!("Basic {basic}"))
        .send_form(&[("grant_type", "client_credentials")])
        .map_err(|e| format!("Spotify: {e}"))?
        .into_json()
        .map_err(|e| e.to_string())?;
    let t = tok["access_token"].as_str().ok_or("Spotify: токен не получен")?;
    let r: serde_json::Value = ureq::get("https://api.spotify.com/v1/search")
        .set("Authorization", &format!("Bearer {t}"))
        .query("q", q)
        .query("type", "track")
        .query("limit", &n.min(10).to_string()) // с февраля 2026 максимум 10
        .call()
        .map_err(|e| format!("Spotify: {e}"))?
        .into_json()
        .map_err(|e| e.to_string())?;
    let items = r["tracks"]["items"].as_array().cloned().unwrap_or_default();
    Ok(items.iter().map(|it| {
        let artist = it["artists"].as_array()
            .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        let title = s(&it["name"]);
        Track {
            id: format!("sp_{}", s(&it["id"])),
            url: format!("https://music.youtube.com/search?q={}", enc(&format!("{artist} {title}"))),
            title, artist,
            duration: it["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0,
            cover: it["album"]["images"][0]["url"].as_str().map(String::from),
            file: None, local_cover: None, source: "sp".into(),
        }
    }).collect())
}

fn one(src: &str, q: &str, n: u32, id: &str, secret: &str) -> Result<Vec<Track>, String> {
    match src {
        "sp" => spotify(q, n, id, secret),
        "sc" => ytdlp_search(format!("scsearch{n}:{q}"), n, "sc"),
        "yt" => ytdlp_search(format!("ytsearch{n}:{q}"), n, "yt"),
        "ytm" => ytdlp_search(format!("https://music.youtube.com/search?q={}", enc(q)), n, "ytm"),
        _ => Err("неизвестный источник".into()),
    }
}

#[tauri::command]
async fn search(query: String, count: u32, source: String, sp_id: String, sp_secret: String, enabled: Vec<String>) -> Result<Vec<Track>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let srcs: Vec<String> = if source == "all" { enabled } else { vec![source] };
        if srcs.is_empty() {
            return Err("Включите хотя бы один источник в настройках".to_string());
        }
        let n = if srcs.len() > 1 { (count / srcs.len() as u32).max(6) } else { count };
        let hs: Vec<_> = srcs.into_iter().map(|src| {
            let (q, i, k) = (query.clone(), sp_id.clone(), sp_secret.clone());
            std::thread::spawn(move || one(&src, &q, n, &i, &k))
        }).collect();
        let (mut lists, mut err) = (vec![], None);
        for h in hs {
            match h.join().unwrap_or_else(|_| Err("поток поиска упал".to_string())) {
                Ok(v) => lists.push(v),
                Err(e) => err = Some(e),
            }
        }
        if lists.is_empty() {
            return Err(err.unwrap_or_default());
        }
        let (mut out, mut i) = (vec![], 0);
        loop {
            let mut any = false;
            for l in &lists {
                if let Some(t) = l.get(i) { out.push(t.clone()); any = true; }
            }
            if !any { break; }
            i += 1;
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn download(app: AppHandle, mut track: Track) -> Result<Track, String> {
    let dir = tracks_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let tpl = dir.join(format!("{}.%(ext)s", track.id));
        let out = ytdlp()
            .args(["-x", "--audio-format", "mp3", "--playlist-items", "1", "--no-warnings"])
            .args(["--write-thumbnail", "--convert-thumbnails", "jpg", "-o"])
            .arg(&tpl)
            .arg(&track.url)
            .output()
            .map_err(|e| format!("yt-dlp не найден: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("ошибка yt-dlp").to_string());
        }
        let mp3 = dir.join(format!("{}.mp3", track.id));
        if !mp3.exists() {
            return Err("файл не создан, проверьте ffmpeg".into());
        }
        let jpg = dir.join(format!("{}.jpg", track.id));
        track.file = Some(mp3.to_string_lossy().into_owned());
        track.local_cover = jpg.exists().then(|| jpg.to_string_lossy().into_owned());
        fs::write(dir.join(format!("{}.json", track.id)), serde_json::to_string(&track).unwrap()).ok();
        Ok(track)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn library(app: AppHandle) -> Result<Vec<Track>, String> {
    let dir = tracks_dir(&app)?;
    let mut items: Vec<(std::time::SystemTime, Track)> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| e.path().extension().map_or(false, |x| x == "json"))
        .filter_map(|e| {
            let t: Track = serde_json::from_str(&fs::read_to_string(e.path()).ok()?).ok()?;
            PathBuf::from(t.file.as_ref()?).exists().then(|| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), t))
        })
        .collect();
    items.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(items.into_iter().map(|x| x.1).collect())
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    type H = isize;
    #[repr(C)]
    struct Pt { x: i32, y: i32 }
    #[link(name = "user32")]
    extern "system" {
        fn FindWindowW(c: *const u16, n: *const u16) -> H;
        fn FindWindowExW(p: H, a: H, c: *const u16, n: *const u16) -> H;
        fn SendMessageW(h: H, m: u32, w: usize, l: isize) -> isize;
        fn GetWindowThreadProcessId(h: H, pid: *mut u32) -> u32;
        fn ClientToScreen(h: H, p: *mut Pt) -> i32;
        fn IsWindowVisible(h: H) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(a: u32, i: i32, pid: u32) -> H;
        fn VirtualAllocEx(p: H, addr: *mut c_void, size: usize, t: u32, prot: u32) -> *mut c_void;
        fn VirtualFreeEx(p: H, addr: *mut c_void, size: usize, t: u32) -> i32;
        fn ReadProcessMemory(p: H, base: *const c_void, buf: *mut c_void, size: usize, read: *mut usize) -> i32;
        fn CloseHandle(h: H) -> i32;
    }
    fn w(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }

    // Прямоугольники всех значков рабочего стола в экранных пикселях: [left, top, right, bottom]
    pub fn icons() -> Vec<[i32; 4]> {
        unsafe {
            let null = std::ptr::null();
            let mut view = FindWindowExW(FindWindowW(w("Progman").as_ptr(), null), 0, w("SHELLDLL_DefView").as_ptr(), null);
            let mut wk = 0;
            while view == 0 {
                wk = FindWindowExW(0, wk, w("WorkerW").as_ptr(), null);
                if wk == 0 { break; }
                view = FindWindowExW(wk, 0, w("SHELLDLL_DefView").as_ptr(), null);
            }
            if view == 0 { return vec![]; }
            let lv = FindWindowExW(view, 0, w("SysListView32").as_ptr(), null);
            if lv == 0 || IsWindowVisible(lv) == 0 { return vec![]; }
            let n = SendMessageW(lv, 0x1004, 0, 0).max(0) as usize; // LVM_GETITEMCOUNT
            if n == 0 { return vec![]; }
            let mut pid = 0u32;
            GetWindowThreadProcessId(lv, &mut pid);
            let proc = OpenProcess(0x38, 0, pid);
            if proc == 0 { return vec![]; }
            let mut out = vec![];
            let buf = VirtualAllocEx(proc, std::ptr::null_mut(), n * 16, 0x3000, 0x04);
            if !buf.is_null() {
                for i in 0..n {
                    SendMessageW(lv, 0x100E, i, (buf as usize + i * 16) as isize); // LVM_GETITEMRECT, LVIR_BOUNDS
                }
                let mut raw = vec![0i32; n * 4];
                let mut rd = 0usize;
                if ReadProcessMemory(proc, buf, raw.as_mut_ptr() as *mut c_void, n * 16, &mut rd) != 0 {
                    for c in raw.chunks(4) {
                        let mut a = Pt { x: c[0], y: c[1] };
                        let mut b = Pt { x: c[2], y: c[3] };
                        ClientToScreen(lv, &mut a);
                        ClientToScreen(lv, &mut b);
                        if b.x > a.x && b.y > a.y { out.push([a.x, a.y, b.x, b.y]); }
                    }
                }
                VirtualFreeEx(proc, buf, 0, 0x8000);
            }
            CloseHandle(proc);
            out
        }
    }
}
#[cfg(not(windows))]
mod win {
    pub fn icons() -> Vec<[i32; 4]> { vec![] }
}

#[tauri::command]
async fn desktop_icons() -> Vec<[i32; 4]> {
    tauri::async_runtime::spawn_blocking(win::icons).await.unwrap_or_default()
}

fn main() {
    tauri::Builder::default()
        .on_window_event(|w, e| {
            if let tauri::WindowEvent::CloseRequested { .. } = e {
                if w.label() == "main" {
                    w.app_handle().exit(0); // скрытый виджет не держит приложение
                }
            }
        })
        .invoke_handler(tauri::generate_handler![search, download, library, desktop_icons])
        .run(tauri::generate_context!())
        .expect("ошибка запуска приложения");
}
