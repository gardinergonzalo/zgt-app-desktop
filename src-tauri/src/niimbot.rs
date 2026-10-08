use base64::{engine::general_purpose::STANDARD, Engine as _};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification, WriteType,
};
use btleplug::platform::{Manager, Peripheral};
use futures_util::{Stream, StreamExt};
use image::imageops::FilterType;
use std::time::{Duration, Instant};
use tauri::WebviewWindow;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const SERVICE_UUID: &str = "E7810A71-73AE-499D-8C15-FAA9AEF0C3F2";
const CHARACTERISTIC_UUID: &str = "BEF8D6C9-9C21-4C9E-B632-BD58C1009F9F";
const MODEL_ID_B1_PRO: u16 = 4097;
const WIDTH: usize = 576;
const HEIGHT: usize = 354;
const DENSITY: u8 = 3;
const LABEL_TYPE: u8 = 1;
const SPEED: u8 = 1;

#[derive(Clone)]
struct Packet {
    command: u8,
    data: Vec<u8>,
}

#[derive(Default)]
struct PacketDecoder {
    buffer: Vec<u8>,
}

impl PacketDecoder {
    fn append(&mut self, chunk: &[u8]) -> Vec<Packet> {
        if !chunk.is_empty() {
            self.buffer.extend_from_slice(chunk);
        }

        let mut packets = Vec::new();

        loop {
            while self.buffer.len() >= 2
                && !(self.buffer[0] == 0x55 && self.buffer[1] == 0x55)
            {
                self.buffer.remove(0);
            }

            if self.buffer.len() < 4 {
                break;
            }

            let command = self.buffer[2];
            let length = self.buffer[3] as usize;
            let frame_length = 7 + length;

            if self.buffer.len() < frame_length {
                break;
            }

            if self.buffer[5 + length] != 0xAA || self.buffer[6 + length] != 0xAA {
                self.buffer.remove(0);
                continue;
            }

            let payload = self.buffer[4..4 + length].to_vec();
            let mut crc = command ^ (length as u8);
            for byte in &payload {
                crc ^= *byte;
            }

            if self.buffer[4 + length] != crc {
                self.buffer.remove(0);
                continue;
            }

            packets.push(Packet {
                command,
                data: payload,
            });
            self.buffer.drain(0..frame_length);
        }

        if self.buffer.len() > 4096 {
            self.buffer.clear();
        }

        packets
    }
}

fn emit(window: &WebviewWindow, event_type: &str, message: &str, connected: bool) {
    let payload = serde_json::json!({
        "type": event_type,
        "message": message
    });

    let script = format!(
        "window.__ZGTNativeNiimbotConnected={};if(window.ZEOZZGTPrintNativeCallback){{window.ZEOZZGTPrintNativeCallback({});}}",
        if connected { "true" } else { "false" },
        payload
    );

    let _ = window.eval(&script);
}

fn pack(command: u8, data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(data.len() + 7);
    packet.extend_from_slice(&[0x55, 0x55, command, data.len() as u8]);

    let mut crc = command ^ (data.len() as u8);
    for byte in data {
        packet.push(*byte);
        crc ^= *byte;
    }

    packet.extend_from_slice(&[crc, 0xAA, 0xAA]);
    packet
}

async fn write_raw(
    peripheral: &Peripheral,
    characteristic: &Characteristic,
    value: &[u8],
) -> Result<(), String> {
    peripheral
        .write(characteristic, value, WriteType::WithResponse)
        .await
        .map_err(|error| format!("La B1 Pro rechazó un paquete Bluetooth: {error}"))
}

async fn send(
    peripheral: &Peripheral,
    characteristic: &Characteristic,
    command: u8,
    data: &[u8],
) -> Result<(), String> {
    write_raw(peripheral, characteristic, &pack(command, data)).await
}

async fn wait_for_response<S>(
    notifications: &mut S,
    decoder: &mut PacketDecoder,
    expected_command: u8,
    duration: Duration,
    step: &str,
) -> Result<Packet, String>
where
    S: Stream<Item = ValueNotification> + Unpin,
{
    let deadline = Instant::now() + duration;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("La B1 Pro no confirmó {step}."));
        }

        match timeout(remaining, notifications.next()).await {
            Ok(Some(notification)) => {
                for packet in decoder.append(&notification.value) {
                    if packet.command == expected_command {
                        return Ok(packet);
                    }
                }
            }
            Ok(None) => {
                return Err("La conexión Bluetooth con la B1 Pro se cerró.".to_string());
            }
            Err(_) => {
                return Err(format!("La B1 Pro no confirmó {step}."));
            }
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
    response: u8,
    duration: Duration,
    step: &str,
) -> Result<Packet, String>
where
    S: Stream<Item = ValueNotification> + Unpin,
{
    send(peripheral, characteristic, command, data).await?;
    wait_for_response(notifications, decoder, response, duration, step).await
}

async fn drain_notifications<S>(
    notifications: &mut S,
    decoder: &mut PacketDecoder,
    duration: Duration,
) where
    S: Stream<Item = ValueNotification> + Unpin,
{
    let deadline = Instant::now() + duration;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match timeout(remaining, notifications.next()).await {
            Ok(Some(notification)) => {
                let _ = decoder.append(&notification.value);
            }
            _ => break,
        }
    }
}

async fn find_printer() -> Result<(Peripheral, Characteristic), String> {
    let manager = Manager::new()
        .await
        .map_err(|error| format!("No se pudo iniciar Bluetooth: {error}"))?;

    let adapters = manager
        .adapters()
        .await
        .map_err(|error| format!("No se pudieron consultar los adaptadores Bluetooth: {error}"))?;

    let adapter = adapters
        .into_iter()
        .next()
        .ok_or_else(|| "No hay un adaptador Bluetooth disponible.".to_string())?;

    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|error| format!("No se pudo buscar la NIIMBOT: {error}"))?;

    sleep(Duration::from_millis(3500)).await;

    let peripherals = adapter
        .peripherals()
        .await
        .map_err(|error| format!("No se pudieron leer los dispositivos Bluetooth: {error}"))?;

    let mut selected: Option<(Peripheral, i16)> = None;

    for peripheral in peripherals {
        let properties = match peripheral.properties().await {
            Ok(Some(properties)) => properties,
            _ => continue,
        };

        let name = properties.local_name.unwrap_or_default().to_uppercase();
        if !name.starts_with("B1") {
            continue;
        }

        let rssi = properties.rssi.unwrap_or(i16::MIN);
        if selected
            .as_ref()
            .map(|(_, best_rssi)| rssi > *best_rssi)
            .unwrap_or(true)
        {
            selected = Some((peripheral, rssi));
        }
    }

    let _ = adapter.stop_scan().await;

    let (peripheral, _) = selected.ok_or_else(|| {
        "No se encontró ninguna NIIMBOT B1 Pro. Verificá que esté encendida y cerca de la Mac.".to_string()
    })?;

    if !peripheral
        .is_connected()
        .await
        .map_err(|error| format!("No se pudo consultar la conexión Bluetooth: {error}"))?
    {
        peripheral
            .connect()
            .await
            .map_err(|error| format!("No se pudo conectar con la NIIMBOT B1 Pro: {error}"))?;
    }

    peripheral
        .discover_services()
        .await
        .map_err(|error| format!("No se pudieron descubrir los servicios de la B1 Pro: {error}"))?;

    let service_uuid = Uuid::parse_str(SERVICE_UUID).map_err(|error| error.to_string())?;
    let characteristic_uuid =
        Uuid::parse_str(CHARACTERISTIC_UUID).map_err(|error| error.to_string())?;

    let service_present = peripheral
        .services()
        .iter()
        .any(|service| service.uuid == service_uuid);

    if !service_present {
        let _ = peripheral.disconnect().await;
        return Err("La impresora no expone el servicio Bluetooth NIIMBOT esperado.".to_string());
    }

    let characteristic = peripheral
        .characteristics()
        .into_iter()
        .find(|item| item.uuid == characteristic_uuid)
        .ok_or_else(|| "No se encontró el canal Bluetooth de impresión NIIMBOT.".to_string())?;

    peripheral
        .subscribe(&characteristic)
        .await
        .map_err(|error| format!("No se pudieron activar las respuestas de la B1 Pro: {error}"))?;

    Ok((peripheral, characteristic))
}

fn prepare_image(data_url: &str) -> Result<Vec<u8>, String> {
    let encoded = data_url
        .strip_prefix("data:image/png;base64,")
        .ok_or_else(|| "La etiqueta enviada por ZGT no tiene un formato PNG válido.".to_string())?;

    let source = STANDARD
        .decode(encoded)
        .map_err(|_| "No se pudo decodificar la etiqueta.".to_string())?;

    let image = image::load_from_memory(&source)
        .map_err(|_| "No se pudo abrir la imagen de la etiqueta.".to_string())?
        .to_rgba8();

    let resized = image::imageops::resize(
        &image,
        WIDTH as u32,
        HEIGHT as u32,
        FilterType::Nearest,
    );

    let stride = (WIDTH + 7) >> 3;
    let mut packed = vec![0u8; stride * HEIGHT];

    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            // En macOS la B1 Pro recibe el bitmap espejado horizontalmente
            // respecto de la imagen fuente. Invertimos X y mantenemos Y como
            // en la v0.1.2 para corregir sólo el espejo sin tocar el protocolo.
            let source_x = WIDTH - 1 - x;
            let source_y = HEIGHT - 1 - y;
            let pixel = resized.get_pixel(source_x as u32, source_y as u32);
            let red = pixel[0] as u32;
            let green = pixel[1] as u32;
            let blue = pixel[2] as u32;
            let alpha = pixel[3] as u32;
            let luminance = (299 * red + 587 * green + 114 * blue) / 1000;

            if alpha > 32 && luminance < 128 {
                let index = y * stride + (x >> 3);
                packed[index] |= 0x80 >> (x & 7);
            }
        }
    }

    Ok(packed)
}

fn row_is_empty(data: &[u8], offset: usize, stride: usize) -> bool {
    data[offset..offset + stride].iter().all(|byte| *byte == 0)
}

fn popcount_row(data: &[u8], offset: usize, stride: usize) -> usize {
    data[offset..offset + stride]
        .iter()
        .map(|byte| byte.count_ones() as usize)
        .sum()
}

async fn send_image(
    window: &WebviewWindow,
    peripheral: &Peripheral,
    characteristic: &Characteristic,
    packed: &[u8],
) -> Result<(), String> {
    let stride = (WIDTH + 7) >> 3;
    let mut row = 0usize;
    let mut last_progress = -10i32;

    while row < HEIGHT {
        let offset = row * stride;
        let empty = row_is_empty(packed, offset, stride);
        let mut run = 1usize;

        while row + run < HEIGHT && run < 200 {
            let next = (row + run) * stride;
            if packed[offset..offset + stride] != packed[next..next + stride] {
                break;
            }
            run += 1;
        }

        if empty {
            let data = [(row >> 8) as u8, row as u8, run as u8];
            send(peripheral, characteristic, 0x84, &data).await?;
        } else {
            let total = popcount_row(packed, offset, stride);
            let mut data = vec![
                (row >> 8) as u8,
                row as u8,
                0,
                total as u8,
                (total >> 8) as u8,
                run as u8,
            ];
            data.extend_from_slice(&packed[offset..offset + stride]);
            send(peripheral, characteristic, 0x85, &data).await?;
        }

        row += run;
        let progress = (row * 100 / HEIGHT) as i32;

        if progress >= last_progress + 10 || row >= HEIGHT {
            last_progress = progress;
            emit(
                window,
                "progress",
                &format!("Enviando etiqueta… {progress}%"),
                true,
            );
        }
    }

    Ok(())
}

pub async fn print_b1_pro(window: WebviewWindow, data_url: String) -> Result<String, String> {
    emit(&window, "progress", "Buscando NIIMBOT B1 Pro…", false);

    let packed = prepare_image(&data_url)?;
    let (peripheral, characteristic) = find_printer().await?;

    let result: Result<String, String> = async {
        let mut notifications = peripheral
            .notifications()
            .await
            .map_err(|error| format!("No se pudieron leer respuestas Bluetooth: {error}"))?;
        let mut decoder = PacketDecoder::default();

        emit(
            &window,
            "connected",
            "B1 Pro conectada. Configurando impresión…",
            true,
        );

        write_raw(
            &peripheral,
            &characteristic,
            &[0x03, 0x55, 0x55, 0xC1, 0x01, 0x01, 0xC1, 0xAA, 0xAA],
        )
        .await?;
        sleep(Duration::from_millis(200)).await;

        let _ = send_wait(
            &peripheral,
            &characteristic,
            &mut notifications,
            &mut decoder,
            0xA5,
            &[1],
            0xB5,
            Duration::from_millis(1500),
            "versión de protocolo",
        )
        .await;

        let model = send_wait(
            &peripheral,
            &characteristic,
            &mut notifications,
            &mut decoder,
            0x40,
            &[0x08],
            0x48,
            Duration::from_secs(2),
            "identificación",
        )
        .await?;

        let model_id = match model.data.as_slice() {
            [high, low, ..] => ((*high as u16) << 8) | (*low as u16),
            [high] => (*high as u16) << 8,
            _ => 0,
        };

        if model_id != MODEL_ID_B1_PRO {
            return Err(format!(
                "La impresora seleccionada no es una NIIMBOT B1 Pro (ID 4097). Detectada: ID {model_id}."
            ));
        }

        send_wait(
            &peripheral,
            &characteristic,
            &mut notifications,
            &mut decoder,
            0x21,
            &[DENSITY],
            0x31,
            Duration::from_secs(2),
            "densidad",
        )
        .await?;

        send_wait(
            &peripheral,
            &characteristic,
            &mut notifications,
            &mut decoder,
            0x23,
            &[LABEL_TYPE],
            0x33,
            Duration::from_secs(2),
            "tipo de etiqueta",
        )
        .await?;

        send_wait(
            &peripheral,
            &characteristic,
            &mut notifications,
            &mut decoder,
            0x01,
            &[0, 1, 0, 0, 0, 0, 0, SPEED, 0],
            0x02,
            Duration::from_secs(3),
            "inicio de impresión",
        )
        .await?;

        // Consulta inicial. Su respuesta se drena para que no pueda validar
        // posteriormente el estado de esta impresión.
        send(&peripheral, &characteristic, 0xA3, &[1]).await?;
        drain_notifications(
            &mut notifications,
            &mut decoder,
            Duration::from_millis(120),
        )
        .await;

        let page_size = [
            (HEIGHT >> 8) as u8,
            HEIGHT as u8,
            (WIDTH >> 8) as u8,
            WIDTH as u8,
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

        send_wait(
            &peripheral,
            &characteristic,
            &mut notifications,
            &mut decoder,
            0x13,
            &page_size,
            0x14,
            Duration::from_secs(3),
            "tamaño de página",
        )
        .await?;

        emit(
            &window,
            "progress",
            "Enviando etiqueta a la B1 Pro…",
            true,
        );
        send_image(&window, &peripheral, &characteristic, &packed).await?;

        send_wait(
            &peripheral,
            &characteristic,
            &mut notifications,
            &mut decoder,
            0xE3,
            &[1],
            0xE4,
            Duration::from_secs(12),
            "fin de página",
        )
        .await?;

        drain_notifications(
            &mut notifications,
            &mut decoder,
            Duration::from_millis(120),
        )
        .await;

        emit(&window, "progress", "Imprimiendo…", true);

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut printed = false;

        while Instant::now() < deadline {
            match send_wait(
                &peripheral,
                &characteristic,
                &mut notifications,
                &mut decoder,
                0xA3,
                &[1],
                0xB3,
                Duration::from_millis(1500),
                "estado de impresión",
            )
            .await
            {
                Ok(status) if status.data.len() >= 4 => {
                    let page = ((status.data[0] as usize) << 8) | status.data[1] as usize;
                    let progress = status.data[2];
                    emit(
                        &window,
                        "progress",
                        &format!("Imprimiendo… {progress}%"),
                        true,
                    );

                    if page >= 1 {
                        printed = true;
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => {}
            }

            sleep(Duration::from_millis(150)).await;
        }

        if !printed {
            return Err("La B1 Pro no confirmó que la etiqueta terminara de imprimirse.".to_string());
        }

        send_wait(
            &peripheral,
            &characteristic,
            &mut notifications,
            &mut decoder,
            0xF3,
            &[1],
            0xF4,
            Duration::from_secs(4),
            "fin de trabajo",
        )
        .await?;

        Ok("Etiqueta impresa y confirmada por la B1 Pro.".to_string())
    }
    .await;

    match &result {
        Ok(message) => emit(&window, "success", message, true),
        Err(message) => emit(&window, "error", message, false),
    }

    let _ = peripheral.unsubscribe(&characteristic).await;
    let _ = peripheral.disconnect().await;

    result
}
