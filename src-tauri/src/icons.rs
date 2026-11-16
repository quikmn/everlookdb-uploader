// Addon icons for the lists: the texture an addon's .toc names (`## IconTexture:`), or else the picture its source
// gives (the catalog's icon, or the GitHub owner's avatar). Each is made into a small PNG once and cached in the
// app's local cache folder (icons/), keyed by the file's path, size and time (or the URL), and handed to the window as a
// data: URL. The game folder is only read.

use crate::{blp, catalog, config};
use image::{imageops, ImageFormat, RgbaImage};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Side of the cached PNGs; the window shows them at 36 px (72 on a 2x screen looks the same).
pub const SIZE: u32 = 64;
/// Bump to rebuild every cached icon (when the way they are made changes).
const VERSION: &str = "1";
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_REMOTE: u64 = 2 * 1024 * 1024;
/// A downloaded icon is fetched again after this long; a failed one is retried after a day.
const REMOTE_FRESH: Duration = Duration::from_secs(14 * 86400);
const REMOTE_RETRY: Duration = Duration::from_secs(86400);
/// Cached icons nobody made in this long are removed (they come back when needed).
const PRUNE_AFTER: Duration = Duration::from_secs(60 * 86400);
const EXTS: [&str; 5] = ["blp", "tga", "png", "jpg", "jpeg"];

/// The file `## IconTexture:` points to, when it is inside `addons_dir`. A file ID (a number) and game textures
/// (`Interface\Icons\...`) live in the game's archives, not on disk: None. Like the game, a path without an
/// extension finds `.blp`, `.tga`, `.png` or `.jpg`.
pub fn resolve(addons_dir: &Path, value: &str) -> Option<PathBuf> {
    let v = value.trim().trim_matches(['"', '\'']).trim();
    if v.is_empty() || v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parts: Vec<&str> = v.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    if parts.len() < 4 || !parts[0].eq_ignore_ascii_case("interface") || !parts[1].eq_ignore_ascii_case("addons") {
        return None;
    }
    let rest = &parts[2..];
    if rest.iter().any(|p| *p == "." || *p == ".." || p.contains(':') || p.contains('\0')) {
        return None;
    }
    let (file, dirs) = rest.split_last()?;
    let dir = dirs.iter().fold(addons_dir.to_path_buf(), |p, d| p.join(d));
    let lower = file.to_ascii_lowercase();
    let stem = match lower.rsplit_once('.') {
        Some((s, ext)) if EXTS.contains(&ext) => {
            let exact = dir.join(file);
            if exact.is_file() {
                return Some(exact);
            }
            &file[..s.len()]
        }
        _ => file,
    };
    EXTS.iter().map(|ext| dir.join(format!("{stem}.{ext}"))).find(|p| p.is_file())
}

/// Decode a texture (BLP, TGA, PNG or JPEG) to RGBA.
pub fn decode(bytes: &[u8], ext: &str) -> Result<RgbaImage, String> {
    if bytes.starts_with(b"BLP") {
        let b = blp::decode(bytes, SIZE)?;
        return RgbaImage::from_raw(b.width, b.height, b.pixels).ok_or_else(|| "BLP size mismatch".to_string());
    }
    // TGA has no signature: go by the extension
    let img = if ext.eq_ignore_ascii_case("tga") {
        image::load_from_memory_with_format(bytes, ImageFormat::Tga)
    } else {
        image::load_from_memory(bytes)
    };
    img.map(|i| i.to_rgba8()).map_err(|e| e.to_string())
}

/// Square `SIZE` icon: transparent margins trimmed, then scaled to fit. Pictures that fill their square (opaque
/// corners) fill the icon; shapes on a transparent background get some room around them. None: nothing visible.
pub fn thumbnail(img: &RgbaImage) -> Option<RgbaImage> {
    let (w, h) = img.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] > 16 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if x0 > x1 {
        return None;
    }
    let mut cut = imageops::crop_imm(img, x0, y0, x1 - x0 + 1, y1 - y0 + 1).to_image();
    let (cw, ch) = cut.dimensions();
    let corners = [(0, 0), (cw - 1, 0), (0, ch - 1), (cw - 1, ch - 1)];
    let full = corners.iter().all(|&(x, y)| cut.get_pixel(x, y)[3] > 200) && cw.abs_diff(ch) * 8 <= cw.max(ch);
    let room = if full { SIZE } else { SIZE - 10 };
    let scale = room as f32 / cw.max(ch) as f32;
    let (nw, nh) = (((cw as f32 * scale).round() as u32).clamp(1, SIZE), ((ch as f32 * scale).round() as u32).clamp(1, SIZE));
    // scale with premultiplied alpha, or the transparent (black) pixels darken the edges
    premultiply(&mut cut, true);
    let filter = if nw < cw { imageops::FilterType::CatmullRom } else { imageops::FilterType::Triangle };
    let mut small = imageops::resize(&cut, nw, nh, filter);
    premultiply(&mut small, false);
    let mut out = RgbaImage::new(SIZE, SIZE);
    imageops::overlay(&mut out, &small, ((SIZE - nw) / 2) as i64, ((SIZE - nh) / 2) as i64);
    Some(out)
}

fn premultiply(img: &mut RgbaImage, on: bool) {
    for p in img.pixels_mut() {
        let a = p[3] as u32;
        for c in 0..3 {
            p[c] = if on { (p[c] as u32 * a / 255) as u8 } else { (p[c] as u32 * 255).checked_div(a).map_or(0, |v| v.min(255) as u8) };
        }
    }
}

fn png(img: &RgbaImage) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

pub fn data_url(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64(png))
}

fn dir() -> PathBuf {
    config::cache_dir().join("icons")
}

fn key(parts: &str) -> String {
    Sha256::digest(format!("{VERSION}|{parts}").as_bytes()).iter().take(10).map(|b| format!("{b:02x}")).collect()
}

fn write(path: &Path, bytes: &[u8]) {
    let _ = std::fs::create_dir_all(dir());
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn age(path: &Path) -> Option<Duration> {
    std::fs::metadata(path).ok()?.modified().ok().map(|t| SystemTime::now().duration_since(t).unwrap_or_default())
}

/// The icon made from a texture in the game folder, as a data: URL (cached by path, size and time).
pub fn local(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let time = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    let k = key(&format!("{}|{}|{time}", path.display(), meta.len()));
    let cached = dir().join(format!("{k}.png"));
    if let Ok(b) = std::fs::read(&cached) {
        return Some(data_url(&b));
    }
    let failed = dir().join(format!("{k}.none"));
    if failed.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let made = std::fs::read(path).ok().and_then(|b| decode(&b, &ext).ok()).and_then(|i| thumbnail(&i)).and_then(|t| png(&t));
    match made {
        Some(b) => {
            write(&cached, &b);
            Some(data_url(&b))
        }
        None => {
            write(&failed, b"");
            None
        }
    }
}

/// The picture the addon's source gives: the catalog's own icon, or the GitHub owner's avatar.
pub fn source_url(a: &catalog::Addon) -> Option<String> {
    if a.icon.starts_with("https://") {
        return Some(a.icon.clone());
    }
    let owner = a.id.strip_prefix("github:")?.split('/').next()?;
    let ok = !owner.is_empty() && owner.len() <= 39 && owner.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    ok.then(|| format!("https://github.com/{owner}.png?size=128"))
}

/// A downloaded icon, as a data: URL (cached for two weeks; a failure is retried after a day).
pub fn remote(url: &str) -> Option<String> {
    if !url.starts_with("https://") {
        return None;
    }
    let k = key(url);
    let cached = dir().join(format!("r{k}.png"));
    let failed = dir().join(format!("r{k}.none"));
    let have = std::fs::read(&cached).ok();
    if have.is_some() && age(&cached).is_some_and(|a| a < REMOTE_FRESH) {
        return have.map(|b| data_url(&b));
    }
    if have.is_none() && age(&failed).is_some_and(|a| a < REMOTE_RETRY) {
        return None;
    }
    let made = fetch(url).ok().and_then(|b| decode(&b, "").ok()).and_then(|i| thumbnail(&i)).and_then(|t| png(&t));
    match (made, have) {
        (Some(b), _) => {
            write(&cached, &b);
            let _ = std::fs::remove_file(&failed);
            Some(data_url(&b))
        }
        // offline: the old one is still good
        (None, Some(b)) => Some(data_url(&b)),
        (None, None) => {
            write(&failed, b"");
            None
        }
    }
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let http = reqwest::blocking::Client::builder()
        .user_agent(concat!("EverlookDB/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .https_only(true)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = http.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("{url} answered {}", resp.status().as_u16()));
    }
    let mut b = Vec::new();
    resp.take(MAX_REMOTE + 1).read_to_end(&mut b).map_err(|e| e.to_string())?;
    if b.len() as u64 > MAX_REMOTE {
        return Err("icon is too large".into());
    }
    Ok(b)
}

/// Remove cached icons not made in a long while (once a run, in the background).
pub fn prune() {
    let Ok(rd) = std::fs::read_dir(dir()) else { return };
    for e in rd.flatten() {
        if age(&e.path()).is_some_and(|a| a > PRUNE_AFTER) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("edb-icons-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn resolves_icon_texture() {
        let addons = tmp("resolve");
        std::fs::create_dir_all(addons.join("Foo/Media")).unwrap();
        std::fs::write(addons.join("Foo/Media/icon.tga"), b"x").unwrap();
        std::fs::write(addons.join("Foo/Media/logo.png"), b"x").unwrap();
        std::fs::write(addons.join("Foo/Media/Prat-3.0.blp"), b"x").unwrap();
        let r = |v: &str| resolve(&addons, v).map(|p| p.strip_prefix(&addons).unwrap().to_string_lossy().replace('\\', "/"));
        assert_eq!(r(r"Interface\AddOns\Foo\Media\icon").as_deref(), Some("Foo/Media/icon.tga"));
        assert_eq!(r("Interface/Addons/Foo/Media/icon").as_deref(), Some("Foo/Media/icon.tga"));
        assert_eq!(r(r"interface\\addons\\Foo\\Media\\logo.png").as_deref(), Some("Foo/Media/logo.png"));
        // a named extension that isn't there: the same name with another one, as the game does
        assert_eq!(r(r"Interface\AddOns\Foo\Media\logo.blp").as_deref(), Some("Foo/Media/logo.png"));
        // a dot in the name isn't an extension
        assert_eq!(r(r"Interface\AddOns\Foo\Media\Prat-3.0").as_deref(), Some("Foo/Media/Prat-3.0.blp"));
        assert_eq!(r(" \"Interface\\AddOns\\Foo\\Media\\icon\" ").as_deref(), Some("Foo/Media/icon.tga"));
        // game files, file IDs, missing files and escapes: no icon
        assert_eq!(r(r"Interface\Icons\INV_Misc_Bag_08"), None);
        assert_eq!(r("134400"), None);
        assert_eq!(r(r"Interface\AddOns\Foo\Media\missing"), None);
        assert_eq!(r(r"Interface\AddOns\Foo\..\..\secret"), None);
        assert_eq!(r(r"Interface\AddOns\C:\x"), None);
        assert_eq!(r(""), None);
        let _ = std::fs::remove_dir_all(&addons);
    }

    /// An uncompressed 32-bit TGA, stored bottom row first (the usual way).
    fn tga(w: u16, h: u16, rows_top_first: &[[u8; 4]]) -> Vec<u8> {
        let mut b = vec![0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.extend_from_slice(&[32, 8]);
        for y in (0..h as usize).rev() {
            for x in 0..w as usize {
                let [r, g, bl, a] = rows_top_first[y * w as usize + x];
                b.extend_from_slice(&[bl, g, r, a]);
            }
        }
        b
    }

    #[test]
    fn decodes_tga_upright() {
        let red = [255, 0, 0, 255];
        let blue = [0, 0, 255, 128];
        let img = decode(&tga(2, 2, &[red, red, blue, blue]), "tga").unwrap();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 0).0, red);
        assert_eq!(img.get_pixel(1, 1).0, blue);
    }

    #[test]
    fn decodes_blp_and_png() {
        let file = blp::tests::blp2(3, 8, 8, 2, 1, &[], &[vec![0, 0, 255, 255, 255, 0, 0, 255]]);
        let img = decode(&file, "blp").unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 255, 255]);
        let png = png(&img).unwrap();
        assert_eq!(decode(&png, "png").unwrap(), img);
        assert!(decode(b"nonsense", "png").is_err());
    }

    #[test]
    fn thumbnails_are_square_and_trimmed() {
        // a 100x50 opaque picture in a transparent 300x300 texture
        let mut img = RgbaImage::new(300, 300);
        for y in 100..150 {
            for x in 50..150 {
                img.put_pixel(x, y, Rgba([200, 100, 50, 255]));
            }
        }
        let t = thumbnail(&img).unwrap();
        assert_eq!(t.dimensions(), (SIZE, SIZE));
        // not square, so it gets room: 54 wide, centred, transparent above and below
        assert_eq!(t.get_pixel(SIZE / 2, SIZE / 2).0, [200, 100, 50, 255]);
        assert_eq!(t.get_pixel(SIZE / 2, 2)[3], 0);
        assert_eq!(t.get_pixel(2, SIZE / 2)[3], 0);
        // a full square picture fills it to the edges
        let sq = RgbaImage::from_pixel(128, 128, Rgba([10, 20, 30, 255]));
        assert_eq!(thumbnail(&sq).unwrap().get_pixel(0, 0).0, [10, 20, 30, 255]);
        // nothing visible: no icon
        assert!(thumbnail(&RgbaImage::new(8, 8)).is_none());
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd, 0x00]), "//79AA==");
    }

    #[test]
    fn source_icons() {
        let a = |id: &str, icon: &str| catalog::Addon { id: id.into(), icon: icon.into(), ..Default::default() };
        assert_eq!(source_url(&a("github:BigWigsMods/BigWigs", "")).as_deref(), Some("https://github.com/BigWigsMods.png?size=128"));
        assert_eq!(source_url(&a("tukui:elvui", "")), None);
        assert_eq!(source_url(&a("tukui:elvui", "https://storage.tukui.org/ElvUI-Logo-Square.png")).as_deref(), Some("https://storage.tukui.org/ElvUI-Logo-Square.png"));
        assert_eq!(source_url(&a("tukui:elvui", "http://insecure.test/x.png")), None);
        assert_eq!(source_url(&a("github:../x", "")), None);
    }

    #[test]
    fn caches_local_icons() {
        let d = tmp("cache");
        config::init_dir(d.join("data"));
        let tex = d.join("icon.tga");
        std::fs::write(&tex, tga(2, 2, &[[255, 0, 0, 255]; 4])).unwrap();
        let url = local(&tex).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        // config::dir may belong to another test's folder (it is set once per test run); whichever it is, the
        // icon is cached there now
        let n = std::fs::read_dir(dir()).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "png")).count();
        assert!(n >= 1);
        assert_eq!(local(&tex).unwrap(), url);
        std::fs::write(d.join("broken.tga"), b"no").unwrap();
        assert_eq!(local(&d.join("broken.tga")), None);
        assert_eq!(local(&d.join("broken.tga")), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
