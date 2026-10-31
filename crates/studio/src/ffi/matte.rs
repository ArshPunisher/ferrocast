//! The person in a picture, found on the CPU by Robust Video Matting (RVM, MobileNetV3, GPL-3.0,
//! github.com/PeterL1n/RobustVideoMatting) through tract, pure Rust: Remove background where
//! Apple's Vision isn't (Windows). The model is a file the installer puts beside OBS's engine
//! (deploy/obs-runtime-windows.ps1); without it the filter isn't offered.
//!
//! RVM is made for video: it carries what it saw in the last frames (its recurrent state), so the
//! mask stays steady from frame to frame. A frame of another size starts afresh.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tract_onnx::prelude::*;
use tract_onnx::tract_core::internal::bail;

/// The model's file name, under the engine's `data/ferrocast/`.
pub(crate) const MODEL: &str = "rvm_mobilenetv3_fp32.onnx";

/// Frames are given to RVM this wide (the height follows the picture's shape): about 20 ms a
/// frame on one core.
pub(crate) const WIDTH: u32 = 384;

/// RVM looks at a frame this much smaller inside (its own advice for small pictures).
const RATIO: f32 = 0.5;

static PATH: OnceLock<PathBuf> = OnceLock::new();

/// Where the model is (FERROCAST_MATTE_MODEL first, else under the engine's data), set once at
/// start → whether there is one.
pub(crate) fn find(obs_data: &str) -> bool {
    let from_env = std::env::var_os("FERROCAST_MATTE_MODEL").map(PathBuf::from);
    // `obs_data` is libobs's own data (`…/data/libobs/`): ours sits beside it.
    let beside = Path::new(obs_data).parent().map(|d| d.join("ferrocast").join(MODEL));
    match from_env.into_iter().chain(beside).find(|p| p.is_file()) {
        Some(p) => PATH.get_or_init(|| p).is_file(),
        None => false,
    }
}

type Plan = std::sync::Arc<TypedRunnableModel>;

/// One picture's matting: the model made for its size, and what it carries between frames.
pub(crate) struct Matte {
    plan: Option<(u32, u32, Plan)>,
    state: Vec<TValue>,
    /// A size the model couldn't be made for: not tried again each frame.
    failed: Option<(u32, u32)>,
}

impl std::fmt::Debug for Matte {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Matte").field("size", &self.plan.as_ref().map(|p| (p.0, p.1))).finish()
    }
}

/// The height frames of `w`×`h` are given at, [`WIDTH`] wide: a multiple of 8, at least 64.
pub(crate) fn height_for(w: u32, h: u32) -> u32 {
    if w == 0 {
        return 64;
    }
    let exact = u64::from(h) * u64::from(WIDTH) / u64::from(w);
    ((exact as u32 + 4) / 8 * 8).clamp(64, 1024)
}

impl Matte {
    pub(crate) fn new() -> Matte {
        Matte { plan: None, state: Vec::new(), failed: None }
    }

    /// The model for `w`×`h`, and RVM's empty state for it (learnt from one plain run).
    fn plan(w: u32, h: u32) -> TractResult<(Plan, Vec<TValue>)> {
        let Some(path) = PATH.get() else { bail!("no model") };
        let (w, h) = (w as usize, h as usize);
        let ratio = || InferenceFact::from(tensor1(&[RATIO]));
        let raw = tract_onnx::onnx().model_for_path(path)?;
        // The state's sizes depend on the frame's: a first, plain run with an empty state shows them.
        let mut first = raw.clone().with_input_fact(0, f32::fact([1, 3, h, w]).into())?;
        for i in 1..=4 {
            first = first.with_input_fact(i, f32::fact([1, 1, 1, 1]).into())?;
        }
        let first = first.with_input_fact(5, ratio())?.into_typed()?.into_runnable()?;
        let zero = || -> TractResult<TValue> { Ok(Tensor::zero::<f32>(&[1, 1, 1, 1])?.into()) };
        let src: TValue = Tensor::zero::<f32>(&[1, 3, h, w])?.into();
        let out = first.run(tvec!(src, zero()?, zero()?, zero()?, zero()?, tensor1(&[RATIO]).into()))?;
        if out.len() < 6 {
            bail!("the model gave {} outputs, not 6", out.len());
        }
        let mut m = raw.with_input_fact(0, f32::fact([1, 3, h, w]).into())?;
        for i in 0..4 {
            m = m.with_input_fact(i + 1, f32::fact(out[2 + i].shape()).into())?;
        }
        let plan = m.with_input_fact(5, ratio())?.into_optimized()?.into_runnable()?;
        let empty = out[2..6]
            .iter()
            .map(|t| Tensor::zero::<f32>(t.shape()).map(TValue::from))
            .collect::<TractResult<Vec<_>>>()?;
        Ok((plan, empty))
    }

    /// The person in an RGBA picture of `w`×`h` (best [`WIDTH`] wide): one byte a pixel, 255
    /// where the person is, at the picture's size.
    pub(crate) fn mask(&mut self, rgba: &[u8], w: u32, h: u32) -> TractResult<Vec<u8>> {
        if w == 0 || h == 0 || rgba.len() != w as usize * h as usize * 4 {
            bail!("a picture of {w}×{h} with {} bytes", rgba.len());
        }
        if self.plan.as_ref().is_none_or(|p| (p.0, p.1) != (w, h)) {
            self.plan = None;
            if self.failed == Some((w, h)) {
                bail!("the model couldn't be made for {w}×{h}");
            }
            let (plan, empty) = Matte::plan(w, h).inspect_err(|_| self.failed = Some((w, h)))?;
            self.plan = Some((w, h, plan));
            self.state = empty;
        }
        let Some((_, _, plan)) = &self.plan else { bail!("no model") };
        let (uw, uh) = (w as usize, h as usize);
        let src: Tensor = tract_ndarray::Array4::from_shape_fn((1, 3, uh, uw), |(_, c, y, x)| {
            f32::from(rgba[(y * uw + x) * 4 + c]) / 255.0
        })
        .into();
        let [r1, r2, r3, r4] = [0, 1, 2, 3].map(|i| self.state[i].clone());
        let out = plan.run(tvec!(src.into(), r1, r2, r3, r4, tensor1(&[RATIO]).into()))?;
        let alpha = out[1].to_plain_array_view::<f32>()?;
        let px: Vec<u8> = alpha.iter().map(|a| (a.clamp(0.0, 1.0) * 255.0).round() as u8).collect();
        self.state = out[2..6].to_vec();
        if px.len() != uw * uh {
            bail!("a mask of {} pixels for {w}×{h}", px.len());
        }
        Ok(px)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_keep_their_shape_in_multiples_of_eight() {
        assert_eq!(height_for(1920, 1080), 216);
        assert_eq!(height_for(1280, 960), 288);
        assert_eq!(height_for(1080, 1920), 680);
        assert_eq!(height_for(4000, 100), 64, "never too thin");
        assert_eq!(height_for(0, 0), 64);
    }
}
