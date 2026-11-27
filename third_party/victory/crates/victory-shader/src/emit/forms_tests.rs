//! Not a test of the translator: which console programs a change to it touches, offline.
//!
//! `VICTORY_FORMS_CONTAINERS=<folder>` and `VICTORY_FORMS_OUT=<file>`: each `*.cdxil` there (a
//! container the user dumped from their own copy of the title, read at test time) is
//! translated for a host of 32-lane waves (`VICTORY_FORMS_WAVE`, default 32), and one line is
//! written per container: its stem, a hash of the translation (the host module and what the
//! host binds for it) and the form a console wave64 compute program took. Two builds' files
//! compared name the programs whose translation changed. Nothing is written but the hashes.

/// FNV-1a, 64 bits: the same for the same bytes on every build.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[test]
fn forms() {
    let (Some(folder), Some(out)) = (
        std::env::var_os("VICTORY_FORMS_CONTAINERS"),
        std::env::var_os("VICTORY_FORMS_OUT"),
    ) else {
        eprintln!("VICTORY_FORMS_CONTAINERS / VICTORY_FORMS_OUT unset; skipped");
        return;
    };
    let wave = std::env::var("VICTORY_FORMS_WAVE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let was = super::lower::TEST_HOST_WAVE.replace(Some(wave));
    let mut paths: Vec<_> = std::fs::read_dir(folder)
        .expect("the folder")
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "cdxil"))
        .collect();
    paths.sort();
    let mut text = String::new();
    for path in &paths {
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let bytes = std::fs::read(path).expect("the container");
        super::take_wave_form();
        let r = super::translate(&bytes);
        let form = super::take_wave_form().map_or("-", |f| f.0);
        let line = match r {
            Ok(shader) => format!(
                "{stem}\t{:016x}\t{form}\n",
                fnv(format!("{shader:?}").as_bytes())
            ),
            Err(e) => format!("{stem}\terror\t{e}\n"),
        };
        text.push_str(&line);
    }
    super::lower::TEST_HOST_WAVE.set(was);
    std::fs::write(out, text).expect("the output file");
    eprintln!("{} containers", paths.len());
}
