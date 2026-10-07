use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tauri::{AppHandle, Manager, State};

#[derive(Default)]
pub struct NiimbotState {
    pub busy: Arc<AtomicBool>,
    pub connected: Arc<AtomicBool>,
    pub cancel: Arc<AtomicBool>,
}

fn emit_native(app: &AppHandle, event_type: &str, message: &str) {
    let payload = serde_json::json!({
        "type": event_type,
        "message": message,
    });

    let script = format!(
        "window.ZEOZZGTPrintNativeCallback && window.ZEOZZGTPrintNativeCallback({});",
        payload
    );

    if let Some(window) = app.get_webview_window("main") {
        let _ = window.eval(script);
    }
}

#[tauri::command]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
pub fn is_niimbot_b1_pro_connected(state: State<'_, NiimbotState>) -> bool {
    state.connected.load(Ordering::Acquire)
}

#[tauri::command]
pub fn disconnect_niimbot_b1_pro(
    app: AppHandle,
    state: State<'_, NiimbotState>,
) -> Result<(), String> {
    state.cancel.store(true, Ordering::Release);
    state.connected.store(false, Ordering::Release);
    emit_native(&app, "disconnected", "B1 Pro desconectada.");
    Ok(())
}

#[tauri::command]
pub fn print_niimbot_b1_pro(
    app: AppHandle,
    state: State<'_, NiimbotState>,
    data_url: String,
) -> Result<(), String> {
    if !data_url.starts_with("data:image/png;base64,") || data_url.len() > 3_000_000 {
        return Err("La etiqueta enviada por ZGT no tiene un formato PNG válido.".to_string());
    }

    if state.busy.swap(true, Ordering::AcqRel) {
        return Err("Ya hay una impresión Bluetooth en curso.".to_string());
    }

    state.cancel.store(false, Ordering::Release);
    state.connected.store(false, Ordering::Release);

    let busy = state.busy.clone();
    let connected = state.connected.clone();
    let cancel = state.cancel.clone();

    #[cfg(target_os = "macos")]
    {
        tauri::async_runtime::spawn(async move {
            let result = macos::print_job(
                app.clone(),
                connected.clone(),
                cancel.clone(),
                data_url,
            )
            .await;

            connected.store(false, Ordering::Release);
            busy.store(false, Ordering::Release);

            if let Err(error) = result {
                emit_native(&app, "error", &error);
            }
        });

        return Ok(());
    }

    #[cfg(not(target_os = "macos"))]
    {
        busy.store(false, Ordering::Release);
        Err("La impresión NIIMBOT nativa está disponible en macOS.".to_string())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::emit_native;
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use btleplug::{
        api::{
            Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
            ValueNotification, WriteType,
        },
        platform::{Manager, Peripheral},
    };
    use futures_util::{Stream, StreamExt};
    use image::{imageops, RgbaImage};
    use std::{
        sync::{atomic::{AtomicBool, Ordering}, Arc},
        time::{Duration, Instant},
    };
    use tauri::AppHandle;
    use tokio::time::{sleep, timeout};
    use uuid::Uuid;

    const SERVICE_UUID: &str = "e7810a71-73ae-499d-8c15-faa9aef0c3f2";
    const CHARACTERISTIC_UUID: &str = "bef8d6c9-9c21-4c9e-b632-bd58c1009f9f";
    const MODEL_ID_B1_PRO: u16 = 4097;
    const WIDTH: u32 = 576;
    const HEIGHT: u32 = 354;
    const DENSITY: u8 = 3;
    const LABEL_TYPE: u8 = 1;
    const SPEED: u8 = 1;

    struct Packet {
        command: u8,
        data: Vec<u8>,
    }

    #[derive(Default)]
    struct PacketDecoder {
        buffer: Vec<u8>,
    }

    impl PacketDecoder {
        fn clear(&mut self) {
            self.buffer.clear();
        }

        fn push(&mut self, bytes: &[u8]) -> Vec<Packet> {
            self.buffer.extend_from_slice(bytes);
            let mut packets = Vec::new();

            loop {
                while self.buffer.len() >= 2
                    && (self.buffer[0] != 0x55 || self.buffer[1] != 0x55)
                {
                    self.buffer.remove(0);
                }

                if self.buffer.len() < 7 {
                    break;
                }

                let length = self.buffer[3] as usize;
                let frame_length = 7 + length;
                if self.buffer.len() < frame_length {
                    break;
                }

                if self.buffer[frame_length - 2] != 0xaa
                    || self.buffer[frame_length - 1] != 0xaa
                {
                    self.buffer.remove(0);
                    continue;
                }

                packets.push(Packet {
                    command: self.buffer[2],
                    data: self.buffer[4..4 + length].to_vec(),
                });
                self.buffer.drain(0..frame_length);
            }

            packets
        }
    }

    fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
        if cancel.load(Ordering::Acquire) {
            Err("Impresión cancelada.".to_string())
        } else {
            Ok(())
        }
    }

    fn pack(command: u8, data: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(7 + data.len());
        output.extend_from_slice(&[0x55, 0x55, command, data.len() as u8]);

        let mut checksum = command ^ data.len() as u8;
        for byte in data {
            output.push(*byte);
            checksum ^= *byte;
        }

        output.extend_from_slice(&[checksum, 0xaa, 0xaa]);
        output
    }

    async fn write_raw(
        peripheral: &Peripheral,
        characteristic: &Characteristic,
        value: &[u8],
    ) -> Result<(), String> {
        if !characteristic.properties.contains(CharPropFlags::WRITE)
            && !characteristic.properties.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
        {
            return Err("La B1 Pro no ofrece un canal de escritura Bluetooth.".to_string());
        }

        let write_type = if characteristic.properties.contains(CharPropFlags::WRITE) {
            WriteType::WithResponse
        } else {
            WriteType::WithoutResponse
        };

        peripheral
            .write(characteristic, value, write_type)
            .await
            .map_err(|error| format!("No se pudo enviar un paquete a la B1 Pro: {error}"))
    }

    async fn wait_for_packet<S>(
        notifications: &mut S,
        decoder: &mut PacketDecoder,
        command: u8,
        timeout_ms: u64,
    ) -> Result<Option<Vec<u8>>, String>
    where
        S: Stream<Item = ValueNotification> + Unpin,
    {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }

            let next = match timeout(remaining, notifications.next()).await {
                Ok(next) => next,
                Err(_) => return Ok(None),
            };

            let Some(notification) = next else {
                return Ok(None);
            };

            for packet in decoder.push(&notification.value) {
                if packet.command == command {
                    return Ok(Some(packet.data));
                }
            }
        }
    }

    async fn drain_available<S>(notifications: &mut S)
    where
        S: Stream<Item = ValueNotification> + Unpin,
    {
        loop {
            match timeout(Duration::from_millis(1), notifications.next()).await {
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
    }

    async fn send_wait<S>(
        peripheral: &Peripheral,
        characteristic: &Characteristic,
        notifications: &mut S,
        decoder: &mut PacketDecoder,
        command: u8,
        data: &[u8],
        response_command: u8,
        timeout_ms: u64,
    ) -> Result<Option<Vec<u8>>, String>
    where
        S: Stream<Item = ValueNotification> + Unpin,
    {
        // Discard notifications generated by the previous command before
        // writing the next one. In particular, B3 status replies from the
        // initial A3 query must never be mistaken for the current page.
        drain_available(notifications).await;
        decoder.clear();
        write_raw(peripheral, characteristic, &pack(command, data)).await?;
        wait_for_packet(notifications, decoder, response_command, timeout_ms).await
    }

    async fn require_ack<S>(
        peripheral: &Peripheral,
        characteristic: &Characteristic,
        notifications: &mut S,
        decoder: &mut PacketDecoder,
        command: u8,
        data: &[u8],
        response_command: u8,
        timeout_ms: u64,
        label: &str,
    ) -> Result<Vec<u8>, String>
    where
        S: Stream<Item = ValueNotification> + Unpin,
    {
        send_wait(
            peripheral,
            characteristic,
            notifications,
            decoder,
            command,
            data,
            response_command,
            timeout_ms,
        )
        .await?
        .ok_or_else(|| format!("La B1 Pro no confirmó {label}."))
    }

    fn image_to_packed(data_url: &str) -> Result<Vec<u8>, String> {
        let encoded = data_url
            .strip_prefix("data:image/png;base64,")
            .ok_or_else(|| "La etiqueta no es una imagen PNG válida.".to_string())?;
        let raw = BASE64
            .decode(encoded)
            .map_err(|_| "La app no pudo decodificar la etiqueta.".to_string())?;
        let source = image::load_from_memory(&raw)
            .map_err(|_| "La app no pudo abrir la imagen de la etiqueta.".to_string())?
            .to_rgba8();
        let bitmap: RgbaImage = if source.width() == WIDTH && source.height() == HEIGHT {
            source
        } else {
            imageops::resize(&source, WIDTH, HEIGHT, imageops::FilterType::Nearest)
        };

        let stride = ((WIDTH + 7) / 8) as usize;
        let mut output = vec![0u8; stride * HEIGHT as usize];

        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let [red, green, blue, alpha] = bitmap.get_pixel(x, y).0;
                if alpha <= 32 {
                    continue;
                }

                let luminance = (299 * red as u16 + 587 * green as u16 + 114 * blue as u16) / 1000;
                if luminance < 128 {
                    let index = y as usize * stride + x as usize / 8;
                    output[index] |= 0x80 >> (x & 7);
                }
            }
        }

        Ok(output)
    }

    fn row_is_empty(buffer: &[u8], offset: usize, stride: usize) -> bool {
        buffer[offset..offset + stride].iter().all(|byte| *byte == 0)
    }

    fn row_popcount(buffer: &[u8], offset: usize, stride: usize) -> u16 {
        buffer[offset..offset + stride]
            .iter()
            .map(|byte| byte.count_ones() as u16)
            .sum()
    }

    async fn send_image(
        app: &AppHandle,
        peripheral: &Peripheral,
        characteristic: &Characteristic,
        cancel: &AtomicBool,
        buffer: &[u8],
    ) -> Result<(), String> {
        let stride = ((WIDTH + 7) / 8) as usize;
        let mut row = 0usize;
        let mut last_progress = 0usize;

        while row < HEIGHT as usize {
            check_cancel(cancel)?;
            let offset = row * stride;
            let empty = row_is_empty(buffer, offset, stride);
            let mut run = 1usize;

            while row + run < HEIGHT as usize && run < 200 {
                let next = (row + run) * stride;
                if buffer[offset..offset + stride] != buffer[next..next + stride] {
                    break;
                }
                run += 1;
            }

            if empty {
                let data = [((row >> 8) & 0xff) as u8, (row & 0xff) as u8, run as u8];
                write_raw(peripheral, characteristic, &pack(0x84, &data)).await?;
            } else {
                let total = row_popcount(buffer, offset, stride);
                let mut data = vec![0u8; 6 + stride];
                data[0] = ((row >> 8) & 0xff) as u8;
                data[1] = (row & 0xff) as u8;
                data[3] = (total & 0xff) as u8;
                data[4] = ((total >> 8) & 0xff) as u8;
                data[5] = run as u8;
                data[6..].copy_from_slice(&buffer[offset..offset + stride]);
                write_raw(peripheral, characteristic, &pack(0x85, &data)).await?;
            }

            row += run;
            let progress = row * 100 / HEIGHT as usize;
            if progress >= last_progress + 10 || row == HEIGHT as usize {
                last_progress = progress;
                emit_native(app, "progress", &format!("Enviando etiqueta… {progress}%"));
            }
        }
        Ok(())
    }

    async fn wait_until_printed<S>(
        app: &AppHandle,
        peripheral: &Peripheral,
        characteristic: &Characteristic,
        notifications: &mut S,
        decoder: &mut PacketDecoder,
        cancel: &AtomicBool,
    ) -> Result<(), String>
    where
        S: Stream<Item = ValueNotification> + Unpin,
    {
        let deadline = Instant::now() + Duration::from_secs(30);

        while Instant::now() < deadline {
            check_cancel(cancel)?;
            if let Some(status) = send_wait(
                peripheral,
                characteristic,
                notifications,
                decoder,
                0xa3,
                &[1],
                0xb3,
                1500,
            )
            .await?
            {
                if status.len() >= 4 {
                    let page = u16::from_be_bytes([status[0], status[1]]);
                    let progress = status[2];
                    emit_native(app, "progress", &format!("Imprimiendo… {progress}%"));
                    if page >= 1 {
                        return Ok(());
                    }
                }
            }

            sleep(Duration::from_millis(150)).await;
        }

        Err("La B1 Pro no confirmó que la etiqueta terminara de imprimirse.".to_string())
    }

    pub async fn print_job(
        app: AppHandle,
        connected: Arc<AtomicBool>,
        cancel: Arc<AtomicBool>,
        data_url: String,
    ) -> Result<(), String> {
        emit_native(&app, "progress", "Buscando NIIMBOT B1 Pro…");

        let manager = Manager::new()
            .await
            .map_err(|error| format!("No se pudo iniciar Bluetooth en macOS: {error}"))?;
        let adapters = manager
            .adapters()
            .await
            .map_err(|error| format!("No se pudo consultar Bluetooth en macOS: {error}"))?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| "Mac no encontró un adaptador Bluetooth disponible.".to_string())?;

        adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(|error| format!("No se pudo buscar la B1 Pro por Bluetooth: {error}"))?;
        sleep(Duration::from_secs(4)).await;
        let peripherals = adapter
            .peripherals()
            .await
            .map_err(|error| format!("No se pudieron leer las impresoras Bluetooth: {error}"))?;
        let _ = adapter.stop_scan().await;

        let mut candidates = Vec::new();
        for peripheral in peripherals {
            if let Ok(Some(properties)) = peripheral.properties().await {
                if let Some(name) = properties.local_name {
                    let uppercase = name.to_uppercase();
                    if uppercase.starts_with("B1") || uppercase.contains("NIIMBOT") {
                        candidates.push((name, peripheral));
                    }
                }
            }
        }

        let (name, peripheral) = candidates
            .into_iter()
            .next()
            .ok_or_else(|| "No se encontró ninguna NIIMBOT B1 Pro. Verificá que esté encendida y cerca de la Mac.".to_string())?;

        check_cancel(&cancel)?;
        emit_native(&app, "progress", &format!("Conectando con {name}…"));
        peripheral
            .connect()
            .await
            .map_err(|error| format!("No se pudo conectar con la B1 Pro: {error}"))?;
        peripheral
            .discover_services()
            .await
            .map_err(|error| format!("No se pudieron consultar los servicios de la B1 Pro: {error}"))?;

        let service_uuid = Uuid::parse_str(SERVICE_UUID).unwrap();
        let characteristic_uuid = Uuid::parse_str(CHARACTERISTIC_UUID).unwrap();
        let characteristic = peripheral
            .characteristics()
            .into_iter()
            .find(|item| item.uuid == characteristic_uuid && item.service_uuid == service_uuid)
            .ok_or_else(|| "La impresora no expone el canal Bluetooth NIIMBOT esperado.".to_string())?;

        peripheral
            .subscribe(&characteristic)
            .await
            .map_err(|error| format!("No se pudieron activar las respuestas de la B1 Pro: {error}"))?;

        let mut notifications = peripheral
            .notifications()
            .await
            .map_err(|error| format!("No se pudo leer el canal de respuestas de la B1 Pro: {error}"))?;
        let mut decoder = PacketDecoder::default();
        connected.store(true, Ordering::Release);
        emit_native(&app, "connected", "B1 Pro conectada. Configurando impresión…");

        let job_result = async {
            check_cancel(&cancel)?;
            write_raw(
                &peripheral,
                &characteristic,
                &[0x03, 0x55, 0x55, 0xc1, 0x01, 0x01, 0xc1, 0xaa, 0xaa],
            )
            .await?;
            sleep(Duration::from_millis(200)).await;
            let _ = send_wait(
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                0xa5,
                &[1],
                0xb5,
                1500,
            )
            .await?;

            let model = require_ack(
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                0x40,
                &[0x08],
                0x48,
                2000,
                "la identificación",
            )
            .await?;
            let model_id = if model.len() >= 2 {
                u16::from_be_bytes([model[0], model[1]])
            } else if model.len() == 1 {
                (model[0] as u16) << 8
            } else {
                0
            };
            if model_id != MODEL_ID_B1_PRO {
                return Err(format!(
                    "La impresora seleccionada no es una NIIMBOT B1 Pro (ID 4097). Detectada: ID {model_id}."
                ));
            }

            require_ack(
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                0x21,
                &[DENSITY],
                0x31,
                2000,
                "la densidad",
            )
            .await?;
            require_ack(
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                0x23,
                &[LABEL_TYPE],
                0x33,
                2000,
                "el tipo de etiqueta",
            )
            .await?;

            let start = [0, 1, 0, 0, 0, 0, 0, SPEED, 0];
            require_ack(
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                0x01,
                &start,
                0x02,
                3000,
                "el inicio de impresión",
            )
            .await?;
            let _ = send_wait(
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                0xa3,
                &[1],
                0xb3,
                1500,
            )
            .await?;

            let page_size = [
                ((HEIGHT >> 8) & 0xff) as u8,
                (HEIGHT & 0xff) as u8,
                ((WIDTH >> 8) & 0xff) as u8,
                (WIDTH & 0xff) as u8,
                0,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ];
            require_ack(
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                0x13,
                &page_size,
                0x14,
                3000,
                "el tamaño de página",
            )
            .await?;

            let packed = image_to_packed(&data_url)?;
            emit_native(&app, "progress", "Enviando etiqueta a la B1 Pro…");
            send_image(
                &app,
                &peripheral,
                &characteristic,
                &cancel,
                &packed,
            )
            .await?;
            require_ack(
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                0xe3,
                &[1],
                0xe4,
                12000,
                "el fin de página",
            )
            .await?;
            emit_native(&app, "progress", "Imprimiendo…");
            wait_until_printed(
                &app,
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                &cancel,
            )
            .await?;
            // PageEnd was received for the current job. Drop any trailing B3
            // status notifications before asking the printer to finish.
            drain_available(&mut notifications).await;
            decoder.clear();
            require_ack(
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                0xf3,
                &[1],
                0xf4,
                4000,
                "el fin de trabajo",
            )
            .await?;
            drain_available(&mut notifications).await;
            Ok::<(), String>(())
        }
        .await;

        let _ = peripheral.disconnect().await;

        job_result?;
        emit_native(&app, "success", "Etiqueta impresa y confirmada por la B1 Pro.");
        Ok(())
    }
}


