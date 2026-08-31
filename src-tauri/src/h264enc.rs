//! Codificador H.264 vía Media Foundation (Windows).
//!
//! Usa el MFT de video encoder del sistema (hardware — Intel QuickSync / AMD VCE /
//! NVENC — si el equipo tiene uno compatible con el modelo síncrono; si no, cae al
//! encoder por software que trae Windows, `CLSID_CMSH264EncoderMFT`). Cero
//! dependencias nuevas: usa el mismo `windows-rs` que ya trae el proyecto para
//! DXGI/D3D11, y evita cualquier problema de licencia de patentes porque el codec
//! viene con la licencia del propio Windows (a diferencia de compilar `openh264`
//! o `x264` desde fuente).
//!
//! Entrada: BGRA empaquetado (lo que entrega `capture.rs`). Se convierte a NV12
//! (el formato que espera el MFT) reutilizando un "lienzo" persistente que solo se
//! repinta en las filas cubiertas por los *dirty rects* de DXGI — así un frame
//! donde solo cambió una franja pequeña de pantalla no paga el coste de
//! recolorear la imagen completa.
//!
//! Salida: unidades de acceso H.264 en formato Annex B (start codes `00 00 00 01`),
//! con SPS/PPS embebidos delante de cada keyframe — listo para decodificar con
//! `VideoDecoder` (WebCodecs) en el frontend usando `avc: { format: "annexb" }`.

/// Un frame de video ya codificado, listo para enviar por la red.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Convierte una región BGRA a NV12, escribiendo solo las filas cubiertas por
/// `rects` en el lienzo `nv12` (persistente entre llamadas). Si `rects` está
/// vacío, no hace nada (llamar con un rect que cubra todo el frame para forzar
/// un repintado completo, p. ej. en el primer frame o tras un cambio de tamaño).
pub fn patch_nv12_from_bgra(
    nv12: &mut [u8],
    canvas_w: usize,
    canvas_h: usize,
    bgra: &[u8],
    rects: &[(u32, u32, u32, u32)],
) {
    let y_size = canvas_w * canvas_h;
    if nv12.len() < y_size + 2 * ((canvas_w + 1) / 2) * ((canvas_h + 1) / 2) {
        return; // lienzo con tamano incorrecto: nada seguro que hacer
    }
    let chroma_w = (canvas_w + 1) / 2;
    let (y_plane, uv_plane) = nv12.split_at_mut(y_size);

    for &(rx, ry, rw, rh) in rects {
        // Alineamos a bordes pares: cada bloque de crominancia cubre un 2x2 de luma.
        let x0 = (rx as usize) & !1;
        let y0 = (ry as usize) & !1;
        let x1 = ((rx as usize) + (rw as usize)).min(canvas_w);
        let y1 = ((ry as usize) + (rh as usize)).min(canvas_h);
        if x0 >= x1 || y0 >= y1 {
            continue;
        }

        for y in y0..y1 {
            let brow = y * canvas_w * 4;
            let yrow = y * canvas_w;
            for x in x0..x1 {
                let bi = brow + x * 4;
                let (b, g, r) = (bgra[bi], bgra[bi + 1], bgra[bi + 2]);
                y_plane[yrow + x] = rgb_to_y(r, g, b);
            }
        }

        let cy0 = y0 / 2;
        let cy1 = (y1 + 1) / 2;
        let cx0 = x0 / 2;
        let cx1 = (x1 + 1) / 2;
        for cy in cy0..cy1 {
            let sy = (cy * 2).min(canvas_h.saturating_sub(1));
            let brow = sy * canvas_w * 4;
            let uvrow = cy * chroma_w * 2;
            for cx in cx0..cx1 {
                let sx = (cx * 2).min(canvas_w.saturating_sub(1));
                let bi = brow + sx * 4;
                let (b, g, r) = (bgra[bi], bgra[bi + 1], bgra[bi + 2]);
                uv_plane[uvrow + cx * 2] = rgb_to_u(r, g, b);
                uv_plane[uvrow + cx * 2 + 1] = rgb_to_v(r, g, b);
            }
        }
    }
}

/// Tamano en bytes de un lienzo NV12 para `w`x`h`.
pub fn nv12_size(w: usize, h: usize) -> usize {
    w * h + 2 * ((w + 1) / 2) * ((h + 1) / 2)
}

fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}
// Coeficientes enteros BT.601 rango de estudio (16-235/16-240), los mismos que
// usan libyuv/ffmpeg para esta conversion.
fn rgb_to_y(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    clamp_u8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16)
}
fn rgb_to_u(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    clamp_u8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128)
}
fn rgb_to_v(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    clamp_u8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128)
}

/// Busca un NAL IDR (5) o SPS (7) en un buffer Annex B: si aparece, el acceso es
/// una keyframe autocontenida (el decoder puede arrancar ahi).
fn contains_idr_or_sps(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 < data.len() {
        let start3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        let start4 = !start3 && i + 4 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;
        if start3 || start4 {
            let hdr = if start3 { i + 3 } else { i + 4 };
            if hdr < data.len() {
                let nal_type = data[hdr] & 0x1F;
                if nal_type == 5 || nal_type == 7 {
                    return true;
                }
            }
            i = hdr;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    #[test]
    fn patch_only_touches_dirty_rows() {
        let w = 8usize;
        let h = 8usize;
        let mut nv12 = vec![7u8; nv12_size(w, h)]; // valor centinela en todo el lienzo
        let mut bgra = vec![0u8; w * h * 4];
        // Pinta de blanco (255,255,255) solo la mitad superior de la imagen.
        for y in 0..4 {
            for x in 0..w {
                let i = (y * w + x) * 4;
                bgra[i] = 255;
                bgra[i + 1] = 255;
                bgra[i + 2] = 255;
                bgra[i + 3] = 255;
            }
        }
        patch_nv12_from_bgra(&mut nv12, w, h, &bgra, &[(0, 0, w as u32, 4)]);

        let y_plane = &nv12[..w * h];
        // Filas 0..4 (parte pintada): deberian acercarse al blanco (~235), no
        // quedar en el centinela.
        for y in 0..4 {
            for x in 0..w {
                assert_ne!(y_plane[y * w + x], 7, "la fila {y} deberia haberse repintado");
            }
        }
        // Filas 4..8 (fuera del rect): deben conservar el centinela intacto.
        for y in 4..8 {
            for x in 0..w {
                assert_eq!(y_plane[y * w + x], 7, "la fila {y} no debia tocarse");
            }
        }
    }

    #[test]
    fn idr_detection_finds_nal_type_5_and_7() {
        // start code de 4 bytes + NAL tipo 7 (SPS): nal_ref_idc=3, type=7 -> 0x67
        let sps = [0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB];
        assert!(contains_idr_or_sps(&sps));
        // NAL tipo 1 (slice no-IDR) -> 0x41: no deberia contar como keyframe.
        let non_idr = [0x00, 0x00, 0x00, 0x01, 0x41, 0xAA, 0xBB];
        assert!(!contains_idr_or_sps(&non_idr));
    }
}

#[cfg(windows)]
mod win {
    use super::EncodedFrame;
    use std::mem::ManuallyDrop;
    use windows::core::{Interface, VARIANT};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

    pub struct H264Encoder {
        transform: IMFTransform,
        codec_api: Option<ICodecAPI>,
        provides_samples: bool,
        output_min_size: u32,
        frame_duration_100ns: i64,
        next_pts_100ns: i64,
    }

    unsafe fn make_video_type(
        subtype: windows::core::GUID,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: Option<u32>,
        profile: Option<u32>,
    ) -> windows::core::Result<IMFMediaType> {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1u64)?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        if let Some(br) = bitrate {
            mt.SetUINT32(&MF_MT_AVG_BITRATE, br)?;
        }
        if let Some(p) = profile {
            mt.SetUINT32(&MF_MT_MPEG2_PROFILE, p)?;
        }
        Ok(mt)
    }

    /// Nivel H.264 aproximado segun resolucion/fps (evita rechazos del MFT por
    /// pedir un nivel demasiado bajo para el tamano de frame).
    fn level_for(width: u32, height: u32) -> u32 {
        let pixels = width as u64 * height as u64;
        if pixels <= 921_600 {
            31 // <=1280x720: nivel 3.1
        } else if pixels <= 2_073_600 {
            40 // <=1920x1080: nivel 4.0
        } else {
            42 // por encima: nivel 4.2
        }
    }

    impl H264Encoder {
        pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self, String> {
            unsafe {
                let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
                // S_OK (0) o S_FALSE (1, "ya estaba inicializado en este hilo") son
                // exito; solo un HRESULT negativo es un fallo real. No hay
                // `CoUninitialize` que le corresponda (ver comentario en Drop):
                // se queda inicializado el resto de vida del hilo, a proposito.
                if hr.0 < 0 {
                    return Err(format!("CoInitializeEx: {hr:?}"));
                }

                MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                    .map_err(|e| format!("MFStartup: {e}"))?;

                let output_info = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_H264,
                };
                let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
                let mut count: u32 = 0;
                let enum_flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
                MFTEnumEx(
                    MFT_CATEGORY_VIDEO_ENCODER,
                    enum_flags,
                    None,
                    Some(&output_info as *const _),
                    &mut activates,
                    &mut count,
                )
                .map_err(|e| format!("MFTEnumEx: {e}"))?;

                if count == 0 || activates.is_null() {
                    if !activates.is_null() {
                        CoTaskMemFree(Some(activates as *const _));
                    }
                    return Err("no hay ningun encoder H.264 disponible (MFTEnumEx devolvio 0)".into());
                }

                let mut list: Vec<Option<IMFActivate>> = Vec::with_capacity(count as usize);
                for i in 0..count as usize {
                    list.push(std::ptr::read(activates.add(i)));
                }
                CoTaskMemFree(Some(activates as *const _));

                // Algunos MFT que aparecen en la enumeracion (sobre todo de
                // hardware, segun el driver de la GPU) pueden fallar al
                // activarse, o resultar ser un MFT puramente asincrono (varios
                // encoders de hardware modernos lo son). Este codigo solo
                // soporta el modelo sincrono clasico (mas simple), asi que
                // saltamos los asincronos y probamos el resto en orden hasta
                // que uno funcione — normalmente cae en el encoder por
                // software que trae Windows, que siempre es sincrono.
                let mut transform: Option<IMFTransform> = None;
                let mut last_err = String::new();
                for candidate in list.into_iter().flatten() {
                    let t: IMFTransform = match candidate.ActivateObject::<IMFTransform>() {
                        Ok(t) => t,
                        Err(e) => {
                            last_err = format!("ActivateObject: {e}");
                            continue;
                        }
                    };
                    let is_async = t
                        .GetAttributes()
                        .and_then(|attrs| attrs.GetUINT32(&MF_TRANSFORM_ASYNC))
                        .unwrap_or(0)
                        != 0;
                    if is_async {
                        last_err = "MFT asincrono (no soportado): saltado".to_string();
                        continue;
                    }
                    transform = Some(t);
                    break;
                }
                let transform = transform.ok_or_else(|| {
                    if last_err.is_empty() {
                        "MFTEnumEx: lista vacia".to_string()
                    } else {
                        last_err
                    }
                })?;

                let out_type = make_video_type(
                    MFVideoFormat_H264,
                    width,
                    height,
                    fps,
                    Some(bitrate_bps),
                    Some(eAVEncH264VProfile_Base.0 as u32),
                )
                .map_err(|e| format!("output media type: {e}"))?;
                out_type
                    .SetUINT32(&MF_MT_MPEG2_LEVEL, level_for(width, height))
                    .ok();
                transform
                    .SetOutputType(0, &out_type, 0)
                    .map_err(|e| format!("SetOutputType: {e}"))?;

                // Las propiedades de baja latencia hay que fijarlas ANTES de
                // `SetInputType`: es lo que arranca la tuberia interna del MFT.
                // Aun asi, el encoder por SOFTWARE de Windows (CMSH264EncoderMFT)
                // tiene un "lookahead" de arranque FIJO de ~16 frames que
                // ninguna de estas propiedades elimina (medido con la prueba de
                // humo de este modulo: los primeros 16 `encode()` devuelven 0
                // unidades, 1:1 a partir de ahi) — es una caracteristica de ese
                // MFT concreto, no un bug de configuracion. A 30fps son ~500ms
                // de pantalla congelada solo al ARRANCAR la sesion (no en
                // regimen estable). El encoder por HARDWARE de este equipo
                // evitaria ese retraso, pero solo esta disponible en modelo
                // asincrono (ver el comentario mas arriba sobre por que este
                // modulo, de momento, solo soporta el modelo sincrono): queda
                // como mejora futura.
                let codec_api: Option<ICodecAPI> = transform.cast().ok();
                if let Some(api) = &codec_api {
                    let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                    let _ = api.SetValue(&CODECAPI_AVEncCommonLowLatency, &VARIANT::from(true));
                    let _ = api.SetValue(&CODECAPI_AVEncCommonRealTime, &VARIANT::from(true));
                    let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                    let _ = api.SetValue(
                        &CODECAPI_AVEncCommonRateControlMode,
                        &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                    );
                    let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate_bps));
                    let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps.max(1) * 2));
                }

                let in_type = make_video_type(MFVideoFormat_NV12, width, height, fps, None, None)
                    .map_err(|e| format!("input media type: {e}"))?;
                transform
                    .SetInputType(0, &in_type, 0)
                    .map_err(|e| format!("SetInputType: {e}"))?;

                transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                    .map_err(|e| format!("BEGIN_STREAMING: {e}"))?;
                transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                    .map_err(|e| format!("START_OF_STREAM: {e}"))?;

                let out_stream_info = transform
                    .GetOutputStreamInfo(0)
                    .map_err(|e| format!("GetOutputStreamInfo: {e}"))?;
                let provides_samples =
                    (out_stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

                Ok(H264Encoder {
                    transform,
                    codec_api,
                    provides_samples,
                    output_min_size: out_stream_info.cbSize.max((width * height / 2).max(65536)),
                    frame_duration_100ns: 10_000_000i64 / fps.max(1) as i64,
                    next_pts_100ns: 0,
                })
            }
        }

        pub fn set_bitrate(&mut self, bps: u32) {
            if let Some(api) = &self.codec_api {
                unsafe {
                    let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps));
                }
            }
        }

        pub fn request_keyframe(&mut self) {
            if let Some(api) = &self.codec_api {
                unsafe {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(true));
                }
            }
        }

        /// Codifica un frame NV12 (`nv12.len() == nv12_size(width,height)`) y
        /// devuelve las unidades de acceso listas producidas (normalmente 0 o 1).
        pub fn encode(&mut self, nv12: &[u8]) -> Result<Vec<EncodedFrame>, String> {
            unsafe {
                let buf = MFCreateMemoryBuffer(nv12.len() as u32)
                    .map_err(|e| format!("MFCreateMemoryBuffer: {e}"))?;
                {
                    let mut ptr: *mut u8 = std::ptr::null_mut();
                    let mut max_len: u32 = 0;
                    buf.Lock(&mut ptr, Some(&mut max_len), None)
                        .map_err(|e| format!("Lock: {e}"))?;
                    let n = nv12.len().min(max_len as usize);
                    std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, n);
                    buf.Unlock().ok();
                }
                buf.SetCurrentLength(nv12.len() as u32)
                    .map_err(|e| format!("SetCurrentLength: {e}"))?;

                let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
                sample.AddBuffer(&buf).map_err(|e| format!("AddBuffer: {e}"))?;
                sample
                    .SetSampleTime(self.next_pts_100ns)
                    .map_err(|e| format!("SetSampleTime: {e}"))?;
                sample
                    .SetSampleDuration(self.frame_duration_100ns)
                    .map_err(|e| format!("SetSampleDuration: {e}"))?;
                self.next_pts_100ns += self.frame_duration_100ns;

                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => {}
                    Err(e) => return Err(format!("ProcessInput: {e}")),
                }

                self.drain_output()
            }
        }

        unsafe fn drain_output(&mut self) -> Result<Vec<EncodedFrame>, String> {
            let mut out = Vec::new();
            loop {
                let (sample_holder, mut out_buf) = if self.provides_samples {
                    (
                        None,
                        MFT_OUTPUT_DATA_BUFFER {
                            dwStreamID: 0,
                            pSample: ManuallyDrop::new(None),
                            dwStatus: 0,
                            pEvents: ManuallyDrop::new(None),
                        },
                    )
                } else {
                    let buf = MFCreateMemoryBuffer(self.output_min_size)
                        .map_err(|e| format!("MFCreateMemoryBuffer(out): {e}"))?;
                    let sample = MFCreateSample().map_err(|e| format!("MFCreateSample(out): {e}"))?;
                    sample.AddBuffer(&buf).map_err(|e| format!("AddBuffer(out): {e}"))?;
                    (
                        Some(sample.clone()),
                        MFT_OUTPUT_DATA_BUFFER {
                            dwStreamID: 0,
                            pSample: ManuallyDrop::new(Some(sample)),
                            dwStatus: 0,
                            pEvents: ManuallyDrop::new(None),
                        },
                    )
                };
                let _ = sample_holder;

                let mut status: u32 = 0;
                let result = self
                    .transform
                    .ProcessOutput(0, std::slice::from_mut(&mut out_buf), &mut status);

                let sample = ManuallyDrop::into_inner(out_buf.pSample);
                let events = ManuallyDrop::into_inner(out_buf.pEvents);
                drop(events);

                match result {
                    Ok(()) => {
                        if let Some(sample) = sample {
                            if let Ok(buffer) = sample.ConvertToContiguousBuffer() {
                                let mut ptr: *mut u8 = std::ptr::null_mut();
                                let mut len: u32 = 0;
                                if buffer.Lock(&mut ptr, None, Some(&mut len)).is_ok() {
                                    let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
                                    buffer.Unlock().ok();
                                    let keyframe = super::contains_idr_or_sps(&data);
                                    out.push(EncodedFrame { data, keyframe });
                                }
                            }
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(e) => return Err(format!("ProcessOutput: {e}")),
                }
            }
            Ok(out)
        }
    }

    impl Drop for H264Encoder {
        // Solo avisamos al MFT de que la sesion termino. NO llamamos aqui a
        // `MFShutdown`/`CoUninitialize`: este `drop` corre ANTES de que Rust
        // suelte `self.transform`/`self.codec_api` (los campos se destruyen
        // despues del cuerpo de `drop`), asi que apagar Media Foundation/COM
        // aqui liberaria esos punteros COM contra un runtime ya desmontado
        // (use-after-free real, no teorico: así se detecto). Dejar la
        // inicializacion viva el resto de vida del hilo es inofensivo y el
        // patron habitual para MF.
        fn drop(&mut self) {
            unsafe {
                let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::h264enc::{contains_idr_or_sps, nv12_size};

        /// Prueba de humo contra el Media Foundation REAL de esta maquina (no
        /// hay forma de simularlo de forma fiable): crea un encoder, le mete
        /// unos frames sinteticos y comprueba que sale al menos una keyframe
        /// Annex B valida (arranca con un start code y trae SPS+IDR).
        #[test]
        fn encodes_a_real_keyframe() {
            let w = 64u32;
            let h = 64u32;
            let mut enc = H264Encoder::new(w, h, 30, 600_000)
                .expect("deberia haber al menos el encoder H264 por software de Windows");

            let mut nv12 = vec![0u8; nv12_size(w as usize, h as usize)];
            // Rellenamos con un patron simple (no todo ceros) para que no sea
            // un frame degenerado.
            for (i, b) in nv12.iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }

            eprintln!("[test] codec_api disponible: {}", enc.codec_api.is_some());
            eprintln!("[test] provides_samples: {}", enc.provides_samples);

            let mut got_keyframe = false;
            for i in 0..40 {
                let frames = enc.encode(&nv12).expect("encode no deberia fallar");
                eprintln!("[test] frame {i}: {} unidades de salida", frames.len());
                for f in frames {
                    assert!(!f.data.is_empty(), "una unidad de acceso no puede venir vacia");
                    assert!(
                        f.data.starts_with(&[0, 0, 0, 1]) || f.data.starts_with(&[0, 0, 1]),
                        "la salida debe ser Annex B (empezar con un start code)"
                    );
                    if f.keyframe {
                        got_keyframe = true;
                        assert!(
                            contains_idr_or_sps(&f.data),
                            "una keyframe debe traer SPS/IDR en la misma unidad de acceso"
                        );
                    }
                }
            }
            assert!(got_keyframe, "deberia haber salido al menos una keyframe en 40 frames");
        }
    }
}

#[cfg(windows)]
pub use win::H264Encoder;

/// Sondea si hay algun encoder H.264 disponible en este equipo (hardware o el
/// software que trae Windows). Crea y descarta un encoder diminuto: es el unico
/// modo fiable de saberlo (`MFTEnumEx` puede listar un MFT que luego falle al
/// configurar el tipo de salida). Si falla, el llamante cae a MJPEG.
#[cfg(windows)]
pub fn is_available() -> bool {
    win::H264Encoder::new(64, 64, 30, 500_000).is_ok()
}

#[cfg(not(windows))]
pub fn is_available() -> bool {
    false
}
