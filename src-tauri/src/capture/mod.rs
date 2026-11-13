pub mod camera;
pub mod sources;
pub mod wgc;

use anyhow::Result;
use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

use crate::video::d3d::Gpu;
use camera::{CameraCapture, CameraDeviceInfo};
use sources::SourceInfo;
use wgc::{Frame, WgcCapture};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Screen,
    Camera,
}

#[derive(Clone, Debug)]
pub enum CaptureTarget {
    Screen(SourceInfo),
    Camera(CameraDeviceInfo),
}

impl CaptureTarget {
    pub fn kind(&self) -> StreamKind {
        match self {
            CaptureTarget::Screen(_) => StreamKind::Screen,
            CaptureTarget::Camera(_) => StreamKind::Camera,
        }
    }

    pub fn refresh_rate(&self) -> u32 {
        match self {
            CaptureTarget::Screen(s) => s.refresh_rate(),
            CaptureTarget::Camera(c) => c.fps,
        }
    }
}

pub enum CaptureSession {
    Wgc(WgcCapture),
    Camera(CameraCapture),
}

impl CaptureSession {
    pub fn start(gpu: &Gpu, target: &CaptureTarget, fps: u32, tx: Sender<Frame>) -> Result<Self> {
        match target {
            CaptureTarget::Screen(src) => Ok(Self::Wgc(WgcCapture::start(gpu, src, fps, tx)?)),
            CaptureTarget::Camera(cam) => Ok(Self::Camera(CameraCapture::start(gpu, cam, fps, tx)?)),
        }
    }

    pub fn initial_size(&self) -> (u32, u32) {
        match self {
            CaptureSession::Wgc(w) => w.initial_size,
            CaptureSession::Camera(c) => c.initial_size,
        }
    }
}
