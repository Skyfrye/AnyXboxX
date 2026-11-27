//! What a ranked pipeline's programs are ([`super`]): per shader, the
//! console program's wave size and the form it took on the host, its VGPRs
//! and LDS, its instructions and of those the image samples, the other
//! image accesses, the LDS operations and the buffer and memory accesses,
//! and the size of the host program. Worked out once per shader, when a
//! line first names it, from what the shader table keeps (the container
//! and the translation).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use victory_shader::gfx10::{self, Encoding};
use victory_shader::native::StageIo;
use victory_shader::{Container, FourCC, HwStage};

use super::{Work, held, shaders, stage};
use crate::pipeline::Shader;

/// Instructions of a console program by what they do.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) all: u32,
    /// `image_sample*` and `image_gather*`.
    pub(super) samples: u32,
    /// Other image instructions: loads, stores, atomics, the ray tests.
    pub(super) images: u32,
    /// LDS (`ds_*`).
    pub(super) lds: u32,
    /// Buffer and memory loads and stores (MUBUF, MTBUF, flat, global,
    /// scratch).
    pub(super) memory: u32,
}

/// [`Counts`] of the instructions `code` reaches; `None` if it does not
/// decode.
pub(super) fn count(code: &[u8]) -> Option<Counts> {
    let mut counts = Counts::default();
    for inst in gfx10::decode(code).ok()? {
        counts.all += 1;
        match inst.encoding {
            Encoding::Mimg => {
                let name = inst.name().unwrap_or("");
                if name.starts_with("image_sample") || name.starts_with("image_gather") {
                    counts.samples += 1;
                } else {
                    counts.images += 1;
                }
            }
            Encoding::Ds => counts.lds += 1,
            Encoding::Mubuf | Encoding::Mtbuf | Encoding::Flat | Encoding::Global | Encoding::Scratch => {
                counts.memory += 1
            }
            _ => {}
        }
    }
    Some(counts)
}

/// The thread group a host program declares and the wave size it asks
/// for (0: any), from its pipeline state validation part (`PSV0`, runtime
/// info: wave sizes at 16 and 20, the group at 36).
pub(super) fn host_group(dxil: &[u8]) -> Option<([u32; 3], u32)> {
    let c = Container::parse(dxil).ok()?;
    let part = c.part(FourCC::PSV0)?;
    let info = part.data.get(4..)?;
    let word = |at: usize| info.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    Some(([word(36)?, word(40)?, word(44)?], word(16)?))
}

/// The form of a console program on the host: "w32" for a program of
/// 32-lane waves; for one of 64-lane waves, "w64", and for a compute one
/// how the host runs it: "w64 on 64" in waves of 64, "w64 paired" with two
/// console lanes a host thread (half the group's threads,
/// `victory_shader::emit::WaveForm::PAIRED`), "w64 halves" as two host
/// waves of 32 a console wave (the forms that exchange between the halves
/// through LDS with group barriers, or give each half its own results).
pub(super) fn form(compute: bool, wave64: bool, console: Option<[u32; 3]>, host: Option<([u32; 3], u32)>) -> &'static str {
    let threads = |g: [u32; 3]| g.iter().product::<u32>();
    match (wave64, compute, console, host) {
        (false, ..) => "w32",
        (true, false, ..) => "w64",
        (true, true, _, Some((_, 64))) => "w64 on 64",
        (true, true, Some(c), Some((h, _))) if 2 * threads(h) == threads(c) => "w64 paired",
        (true, true, Some(_), Some(_)) => "w64 halves",
        _ => "w64",
    }
}

/// One shader's properties.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Props {
    pub(super) stage: &'static str,
    pub(super) form: &'static str,
    pub(super) vgprs: u16,
    /// LDS bytes a group (compute and mesh programs).
    pub(super) lds: u32,
    pub(super) counts: Option<Counts>,
    /// Bytes of the host program.
    pub(super) dxil: usize,
    /// The program's name in its container (`Shader::title_name`), and
    /// whether it is one of the ray-tracing structure builder's.
    pub(super) name: Option<String>,
    pub(super) builder: bool,
}

impl Props {
    /// As the line says it: "cs w64 paired v128 lds 8192 isa 1234 smp 6
    /// img 2 ds 40 mem 30 dxil 61k".
    pub(super) fn text(&self) -> String {
        let mut text = format!("{} {} v{}", self.stage, self.form, self.vgprs);
        if self.lds > 0 {
            let _ = write!(text, " lds {}", self.lds);
        }
        if let Some(c) = self.counts {
            let _ = write!(text, " isa {}", c.all);
            for (n, name) in [(c.samples, "smp"), (c.images, "img"), (c.lds, "ds"), (c.memory, "mem")] {
                if n > 0 {
                    let _ = write!(text, " {name} {n}");
                }
            }
        }
        let _ = write!(text, " dxil {}k", self.dxil.div_ceil(1024));
        if self.builder {
            text.push_str(" builder");
        }
        if let Some(name) = &self.name {
            let _ = write!(text, " {name}");
        }
        text
    }
}

/// [`Props`] of `shader`: its console program (the vertex, pixel, compute
/// or mesh one, as the translator takes) and its translation.
fn of(shader: &Shader) -> Option<Props> {
    let programs = shader.programs()?;
    let program = programs
        .iter()
        .find(|p| matches!(p.stage, HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms))?;
    let translated = shader.translated();
    let compute = matches!(program.io, StageIo::Cs(_));
    let lds = match &program.io {
        StageIo::Cs(cs) => cs.lds_bytes(),
        StageIo::Ms(ms) => ms.lds_bytes,
        _ => 0,
    };
    let host = translated.and_then(|t| host_group(&t.host.dxil));
    Some(Props {
        stage: stage(shader.stage),
        form: form(compute, program.wave64, translated.and_then(|t| t.threads), host),
        vgprs: program.vgprs,
        lds,
        counts: count(&program.code),
        dxil: translated.map_or(0, |t| t.host.dxil.len()),
        name: shader.title_name(),
        builder: programs.iter().any(victory_shader::spill::is_spilled),
    })
}

/// Worked out so far, by shader (shaders are never let go).
static KNOWN: Mutex<Option<HashMap<usize, Option<Props>>>> = Mutex::new(None);

/// What the programs of `work` are, for the line ("" for work that is not
/// a pipeline's).
pub(super) fn text(work: &Work) -> String {
    let texts: Vec<String> = shaders(work)
        .map(|s| {
            let props = {
                let mut known = held(&KNOWN);
                known.get_or_insert_with(HashMap::new).get(&(Arc::as_ptr(s) as usize)).cloned()
            };
            let props = props.unwrap_or_else(|| {
                let props = of(s);
                held(&KNOWN).get_or_insert_with(HashMap::new).insert(Arc::as_ptr(s) as usize, props.clone());
                props
            });
            props.map_or_else(|| format!("{} ?", stage(s.stage)), |p| p.text())
        })
        .collect();
    texts.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host form of a console wave64 compute program, from the group
    /// its translation declares: half the threads is the paired form, a
    /// wave size of 64 is the host's own, anything else two host waves.
    #[test]
    fn a_wave64_programs_form_comes_from_its_host_group() {
        let console = Some([64, 1, 1]);
        assert_eq!(form(true, true, console, Some(([32, 1, 1], 0))), "w64 paired");
        assert_eq!(form(true, true, console, Some(([64, 1, 1], 0))), "w64 halves");
        assert_eq!(form(true, true, console, Some(([64, 1, 1], 64))), "w64 on 64");
        assert_eq!(form(true, true, Some([8, 8, 2]), Some(([8, 4, 2], 32))), "w64 paired");
        assert_eq!(form(true, true, console, None), "w64");
        // A vertex or pixel program of 64-lane waves; a compute one of 32.
        assert_eq!(form(false, true, None, None), "w64");
        assert_eq!(form(true, false, console, Some(([64, 1, 1], 0))), "w32");
    }

    /// The group and wave size come from the host program's `PSV0` part, as
    /// the translator writes it.
    #[test]
    fn the_host_group_is_read_from_the_validation_part() {
        // A container of one PSV0 part: the runtime info's size, then the
        // info with a wave size of 32 and a group of 8x4x2.
        let mut info = vec![0u8; 48];
        info[16..20].copy_from_slice(&32u32.to_le_bytes());
        info[20..24].copy_from_slice(&32u32.to_le_bytes());
        for (k, n) in [8u32, 4, 2].into_iter().enumerate() {
            info[36 + 4 * k..40 + 4 * k].copy_from_slice(&n.to_le_bytes());
        }
        let mut part = 48u32.to_le_bytes().to_vec();
        part.extend(info);
        let mut c = b"DXBC".to_vec();
        c.extend([0u8; 16]);
        c.extend(1u16.to_le_bytes());
        c.extend(0u16.to_le_bytes());
        let total = 0x20 + 4 + 8 + part.len();
        c.extend((total as u32).to_le_bytes());
        c.extend(1u32.to_le_bytes());
        c.extend(0x24u32.to_le_bytes());
        c.extend(b"PSV0");
        c.extend((part.len() as u32).to_le_bytes());
        c.extend(&part);
        assert_eq!(host_group(&c), Some(([8, 4, 2], 32)));
        assert_eq!(host_group(b"not a container"), None);
    }

    /// Instructions are counted as the decoder reaches them: `s_nop 0`,
    /// then `s_endpgm`.
    #[test]
    fn a_programs_instructions_are_counted() {
        let code: Vec<u8> = [0xbf80_0000u32, 0xbf81_0000].iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(count(&code), Some(Counts { all: 2, ..Counts::default() }));
        assert_eq!(count(&[0, 0]), None);
    }

    #[test]
    fn a_shaders_properties_read_short() {
        let p = Props {
            stage: "cs",
            form: "w64 paired",
            vgprs: 128,
            lds: 8192,
            counts: Some(Counts { all: 1234, samples: 6, images: 0, lds: 40, memory: 30 }),
            dxil: 61 * 1024 - 10,
            name: None,
            builder: false,
        };
        assert_eq!(p.text(), "cs w64 paired v128 lds 8192 isa 1234 smp 6 ds 40 mem 30 dxil 61k");
        let named = Props { name: Some("CS_Trace_1234_Wrapped".to_owned()), builder: true, ..p };
        assert_eq!(named.text(), "cs w64 paired v128 lds 8192 isa 1234 smp 6 ds 40 mem 30 dxil 61k builder CS_Trace_1234_Wrapped");
    }
}
