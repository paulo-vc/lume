//! Encoder H.264 pela placa de vídeo via Media Foundation (NVENC / Quick Sync / AMF).
//! Entrada NV12 em memória do sistema; saída Annex B (igual ao OpenH264), então o resto do
//! pipeline não muda. Se nada funcionar, quem chama cai para o OpenH264.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};

fn var_u4(v: u32) -> VARIANT {
    VARIANT { Anonymous: VARIANT_0 { Anonymous: ManuallyDrop::new(VARIANT_0_0 { vt: VT_UI4, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: VARIANT_0_0_0 { ulVal: v } }) } }
}
fn var_bool(v: bool) -> VARIANT {
    let b = windows::Win32::Foundation::VARIANT_BOOL(if v { -1 } else { 0 });
    VARIANT { Anonymous: VARIANT_0 { Anonymous: ManuallyDrop::new(VARIANT_0_0 { vt: VT_BOOL, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: VARIANT_0_0_0 { boolVal: b } }) } }
}

const CODECAPI_AVENCCOMMONRATECONTROLMODE: GUID = GUID::from_u128(0x1c0608e9_370c_4710_8a58_cb6181c42423);
const CODECAPI_AVENCMPVGOPSIZE: GUID = GUID::from_u128(0x95f31b26_95a4_41aa_9303_246a7fc6eef1);
const CODECAPI_AVENCMPVDEFAULTBPICTURECOUNT: GUID = GUID::from_u128(0x8d390aac_dc5c_4200_b57f_814d04babab2);

pub struct HwEncoder {
    mft: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    width: usize,
    height: usize,
    frame_hns: i64,
    ts: i64,
    need_input: u32,
    in_id: u32,
    out_id: u32,
    nv12: Vec<u8>,
    pub name: String,
    /// quando recebe texturas da placa de vídeo direto (sem passar pela CPU)
    dxgi: Option<IMFDXGIDeviceManager>,
}

// Usado só pela thread do encoder.
unsafe impl Send for HwEncoder {}

fn init_mf() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        ONCE.call_once(|| {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
        });
    }
}

/// Lista os encoders H.264 por hardware disponíveis (nome amigável).
pub fn hardware_encoders() -> Vec<(String, IMFActivate)> {
    init_mf();
    let mut out = vec![];
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, Some(&input), Some(&output), &mut arr, &mut n).is_err() || arr.is_null() {
            return out;
        }
        for i in 0..n as usize {
            if let Some(a) = (*arr.add(i)).take() {
                let mut s = PWSTR::null();
                let mut len = 0u32;
                let name = if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut s, &mut len).is_ok() {
                    let r = s.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(s.0 as _));
                    r
                } else {
                    "Encoder de hardware".into()
                };
                out.push((name, a));
            }
        }
        CoTaskMemFree(Some(arr as _));
    }
    out
}

impl HwEncoder {
    /// Tenta cada encoder de hardware até um aceitar a configuração.
    /// Com `dev`, o encoder lê texturas NV12 desse dispositivo (caminho todo na GPU).
    pub fn new(width: usize, height: usize, fps: u32, bitrate_bps: u32, gop: u32, dev: Option<&ID3D11Device>) -> Result<Self, String> {
        let mut last = "nenhum encoder de hardware encontrado".to_string();
        for (name, act) in hardware_encoders() {
            match unsafe { Self::open(&act, &name, width, height, fps, bitrate_bps, gop, dev) } {
                Ok(e) => return Ok(e),
                Err(e) => {
                    last = format!("{name}: {e}");
                    unsafe {
                        let _ = act.ShutdownObject();
                    }
                }
            }
        }
        Err(last)
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn open(act: &IMFActivate, name: &str, width: usize, height: usize, fps: u32, bitrate: u32, gop: u32, dev: Option<&ID3D11Device>) -> windows::core::Result<Self> {
        let mft: IMFTransform = act.ActivateObject()?;
        let attrs = mft.GetAttributes()?;
        let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
        if is_async {
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
        }
        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        let dxgi = match dev {
            Some(dev) => {
                if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                    return Err(windows::core::Error::new(E_NOTIMPL, "encoder sem suporte a texturas D3D11"));
                }
                let mut token = 0u32;
                let mut mgr = None;
                MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
                let mgr = mgr.unwrap();
                mgr.ResetDevice(dev, token)?;
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)?;
                Some(mgr)
            }
            None => None,
        };

        let (mut ins, mut outs) = ([0u32], [0u32]);
        let (in_id, out_id) = if mft.GetStreamIDs(&mut ins, &mut outs).is_ok() { (ins[0], outs[0]) } else { (0, 0) };

        let codec: Option<ICodecAPI> = mft.cast().ok();
        if let Some(c) = &codec {
            let _ = c.SetValue(&CODECAPI_AVLowLatencyMode, &var_bool(true));
            let _ = c.SetValue(&CODECAPI_AVENCCOMMONRATECONTROLMODE, &var_u4(eAVEncCommonRateControlMode_CBR.0 as u32));
            let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u4(bitrate));
            let _ = c.SetValue(&CODECAPI_AVENCMPVGOPSIZE, &var_u4(gop));
            let _ = c.SetValue(&CODECAPI_AVENCMPVDEFAULTBPICTURECOUNT, &var_u4(0));
        }

        let size = ((width as u64) << 32) | height as u64;
        let rate = ((fps as u64) << 32) | 1;
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
        out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
        out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // Baseline: o convidado negocia 42e01f (constrained baseline), sem quadros B.
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
        mft.SetOutputType(out_id, &out, 0)?;

        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        inp.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
        inp.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
        inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        mft.SetInputType(in_id, &inp, 0)?;

        mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

        let events = if is_async { Some(mft.cast::<IMFMediaEventGenerator>()?) } else { None };
        Ok(Self {
            mft,
            events,
            codec,
            width,
            height,
            frame_hns: 10_000_000 / fps.max(1) as i64,
            ts: 0,
            need_input: if is_async { 0 } else { u32::MAX },
            in_id,
            out_id,
            nv12: vec![0; width * height * 3 / 2],
            name: name.to_string(),
            dxgi,
        })
    }

    pub fn force_keyframe(&self) {
        if let Some(c) = &self.codec {
            unsafe {
                let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &var_u4(1));
            }
        }
    }

    /// Muda o bitrate médio (CBR) sem reiniciar o encoder.
    pub fn set_bitrate(&self, bps: u32) -> bool {
        match &self.codec {
            Some(c) => unsafe { c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u4(bps)).is_ok() },
            None => false,
        }
    }

    pub fn uses_gpu(&self) -> bool {
        self.dxgi.is_some()
    }

    /// Codifica um quadro BGRA (já no tamanho do encoder). Devolve o que saiu (pode ser vazio).
    pub fn encode(&mut self, bgra: &[u8]) -> Result<Vec<u8>, String> {
        let e = |x: windows::core::Error| x.message();
        bgra_to_nv12(bgra, self.width, self.height, &mut self.nv12);
        let sample = unsafe {
            let buf = MFCreateMemoryBuffer(self.nv12.len() as u32).map_err(e)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None).map_err(e)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), p, self.nv12.len());
            buf.Unlock().map_err(e)?;
            buf.SetCurrentLength(self.nv12.len() as u32).map_err(e)?;
            let sample = MFCreateSample().map_err(e)?;
            sample.AddBuffer(&buf).map_err(e)?;
            sample
        };
        self.submit(sample)
    }

    /// Codifica uma textura NV12 que já está na placa de vídeo (tamanho do encoder).
    pub fn encode_texture(&mut self, tex: &ID3D11Texture2D) -> Result<Vec<u8>, String> {
        let e = |x: windows::core::Error| x.message();
        let sample = unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false).map_err(e)?;
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                if let Ok(n) = b2.GetContiguousLength() {
                    buf.SetCurrentLength(n).map_err(e)?;
                }
            }
            let sample = MFCreateSample().map_err(e)?;
            sample.AddBuffer(&buf).map_err(e)?;
            sample
        };
        self.submit(sample)
    }

    fn submit(&mut self, sample: IMFSample) -> Result<Vec<u8>, String> {
        let e = |x: windows::core::Error| x.message();
        let mut out = vec![];
        unsafe {
            if self.events.is_some() {
                // Assíncrono: espera o encoder pedir entrada (no máximo ~40 ms, senão descarta o quadro).
                self.pump(&mut out, Duration::ZERO).map_err(e)?;
                if self.need_input == 0 {
                    self.pump_until(&mut out, Duration::from_millis(40), |s| s.need_input > 0).map_err(e)?;
                }
                if self.need_input == 0 {
                    return Ok(out);
                }
                self.need_input -= 1;
            }
            sample.SetSampleTime(self.ts).map_err(e)?;
            sample.SetSampleDuration(self.frame_hns).map_err(e)?;
            self.ts += self.frame_hns;
            self.mft.ProcessInput(self.in_id, &sample, 0).map_err(e)?;

            if self.events.is_some() {
                // Pega a saída deste quadro já agora, para não somar um quadro de atraso.
                let before = out.len();
                self.pump_until(&mut out, Duration::from_millis(12), |_| false).map_err(e).map(|_| ())?;
                let _ = before;
            } else {
                while self.drain_one(&mut out).map_err(e)? {}
            }
        }
        Ok(out)
    }

    /// Processa os eventos pendentes sem bloquear.
    unsafe fn pump(&mut self, out: &mut Vec<u8>, _: Duration) -> windows::core::Result<bool> {
        let ev = self.events.clone().unwrap();
        let mut got_output = false;
        loop {
            match ev.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(m) => {
                    let t = m.GetType()?;
                    if t == METransformNeedInput.0 as u32 {
                        self.need_input += 1;
                    } else if t == METransformHaveOutput.0 as u32 {
                        self.drain_one(out)?;
                        got_output = true;
                    }
                }
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(got_output),
                Err(err) => return Err(err),
            }
        }
    }

    unsafe fn pump_until(&mut self, out: &mut Vec<u8>, max: Duration, done: impl Fn(&Self) -> bool) -> windows::core::Result<()> {
        let t = Instant::now();
        loop {
            let got = self.pump(out, Duration::ZERO)?;
            if done(self) || (got && !out.is_empty()) || t.elapsed() >= max {
                return Ok(());
            }
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    /// Lê uma saída. Devolve false se o encoder precisa de mais entrada.
    unsafe fn drain_one(&mut self, out: &mut Vec<u8>) -> windows::core::Result<bool> {
        let info = self.mft.GetOutputStreamInfo(self.out_id)?;
        let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
        let mine = if provides {
            None
        } else {
            let s = MFCreateSample()?;
            s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?)?;
            Some(s)
        };
        let mut db = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: self.out_id, pSample: ManuallyDrop::new(mine), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
        let mut status = 0u32;
        let r = self.mft.ProcessOutput(0, &mut db, &mut status);
        let sample = ManuallyDrop::take(&mut db[0].pSample);
        drop(ManuallyDrop::take(&mut db[0].pEvents));
        match r {
            Ok(()) => {
                if let Some(s) = sample {
                    let b = s.ConvertToContiguousBuffer()?;
                    let mut p = std::ptr::null_mut();
                    let mut len = 0u32;
                    b.Lock(&mut p, None, Some(&mut len))?;
                    out.extend_from_slice(std::slice::from_raw_parts(p, len as usize));
                    b.Unlock()?;
                }
                Ok(true)
            }
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // Renegocia o tipo de saída que o encoder propõe.
                if let Ok(t) = self.mft.GetOutputAvailableType(self.out_id, 0) {
                    self.mft.SetOutputType(self.out_id, &t, 0)?;
                }
                Ok(true)
            }
            Err(err) => Err(err),
        }
    }
}

impl Drop for HwEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(ev) = &self.events {
                if let Ok(s) = ev.cast::<IMFShutdown>() {
                    let _ = s.Shutdown();
                }
            }
        }
    }
}

/// BGRA -> NV12 (BT.601, faixa limitada), mesmo padrão que o OpenH264 usa.
pub fn bgra_to_nv12(bgra: &[u8], w: usize, h: usize, out: &mut [u8]) {
    let (y_plane, uv) = out.split_at_mut(w * h);
    for (row, yrow) in y_plane.chunks_exact_mut(w).enumerate().take(h) {
        let src = &bgra[row * w * 4..(row + 1) * w * 4];
        for (x, y) in yrow.iter_mut().enumerate() {
            let (b, g, r) = (src[x * 4] as i32, src[x * 4 + 1] as i32, src[x * 4 + 2] as i32);
            *y = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        }
    }
    for cy in 0..h / 2 {
        let r0 = &bgra[(cy * 2) * w * 4..];
        let r1 = &bgra[(cy * 2 + 1) * w * 4..];
        let dst = &mut uv[cy * w..cy * w + w];
        for cx in 0..w / 2 {
            let i = cx * 8;
            let b = (r0[i] as i32 + r0[i + 4] as i32 + r1[i] as i32 + r1[i + 4] as i32) / 4;
            let g = (r0[i + 1] as i32 + r0[i + 5] as i32 + r1[i + 1] as i32 + r1[i + 5] as i32) / 4;
            let r = (r0[i + 2] as i32 + r0[i + 6] as i32 + r1[i + 2] as i32 + r1[i + 6] as i32) / 4;
            dst[cx * 2] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            dst[cx * 2 + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn hw_encode_synthetic() {
        let list = hardware_encoders();
        println!("encoders: {:?}", list.iter().map(|x| &x.0).collect::<Vec<_>>());
        let (w, h) = (1920, 1080);
        let mut enc = HwEncoder::new(w, h, 60, 8_000_000, 180, None).expect("encoder de hardware");
        println!("usando: {}", enc.name);
        let mut frame = vec![0u8; w * h * 4];
        let (mut bytes, mut nonempty, mut idr) = (0, 0, 0);
        let t = Instant::now();
        for i in 0..120 {
            for (j, px) in frame.chunks_exact_mut(4).enumerate() {
                px[0] = ((j + i * 7) % 255) as u8;
                px[1] = ((j / w + i) % 255) as u8;
                px[2] = (i * 2 % 255) as u8;
            }
            if i == 60 {
                enc.force_keyframe();
            }
            let o = enc.encode(&frame).unwrap();
            if !o.is_empty() {
                nonempty += 1;
                bytes += o.len();
                // NAL tipo 5 = IDR
                if o.windows(5).any(|s| s[..4] == [0, 0, 0, 1] && s[4] & 0x1f == 5) || o.windows(4).any(|s| s[..3] == [0, 0, 1] && s[3] & 0x1f == 5) {
                    idr += 1;
                }
            }
        }
        let el = t.elapsed();
        println!("{nonempty}/120 quadros com saída, {idr} IDR, {} kB, {:.1} ms/quadro", bytes / 1024, el.as_secs_f64() * 1000.0 / 120.0);
        assert!(nonempty > 100);
        assert!(idr >= 1);
    }
}
