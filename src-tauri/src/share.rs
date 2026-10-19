use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::{fs, path::{Path, PathBuf}};
use tauri::WebviewWindow;

fn decode_pdf_data_url(data_url: &str) -> Result<Vec<u8>, String> {
    let encoded = data_url
        .strip_prefix("data:application/pdf;base64,")
        .or_else(|| data_url.strip_prefix("data:application/octet-stream;base64,"))
        .ok_or_else(|| "El PDF recibido no tiene un formato válido.".to_string())?;

    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "No se pudo decodificar el PDF para compartir.".to_string())?;

    if bytes.len() < 5 || &bytes[..5] != b"%PDF-" {
        return Err("El archivo recibido no es un PDF válido.".to_string());
    }

    Ok(bytes)
}

fn safe_filename(value: &str) -> String {
    let mut name: String = value
        .trim()
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    if name.is_empty() {
        name = "ZGT-Presupuesto.pdf".to_string();
    }

    if !name.to_ascii_lowercase().ends_with(".pdf") {
        name.push_str(".pdf");
    }

    if name.len() > 180 {
        name.truncate(176);
        if !name.to_ascii_lowercase().ends_with(".pdf") {
            name.push_str(".pdf");
        }
    }

    name
}

fn write_temp_pdf(data_url: &str, filename: &str) -> Result<PathBuf, String> {
    let bytes = decode_pdf_data_url(data_url)?;
    let dir = std::env::temp_dir().join("ZGT").join("share");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("No se pudo preparar el PDF para compartir: {error}"))?;

    let path = dir.join(safe_filename(filename));
    fs::write(&path, bytes)
        .map_err(|error| format!("No se pudo guardar temporalmente el PDF: {error}"))?;

    path.canonicalize()
        .map_err(|error| format!("No se pudo preparar la ruta del PDF: {error}"))
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::{
        cell::RefCell,
        ffi::c_void,
        sync::{Arc, Mutex},
    };
    use windows::{
        ApplicationModel::DataTransfer::{DataPackageOperation, DataTransferManager},
        Storage::{IStorageItem, StorageFile},
        Win32::{
            Foundation::HWND,
            UI::Shell::IDataTransferManagerInterop,
        },
        core::{factory, HSTRING, Interface},
    };
    use windows_collections::IIterable;

    struct ShareContext {
        hwnd: isize,
        interop: IDataTransferManagerInterop,
        _manager: DataTransferManager,
        _revoker: windows::core::EventRevoker,
        current_file: Arc<Mutex<Option<StorageFile>>>,
    }

    thread_local! {
        static SHARE_CONTEXT: RefCell<Option<ShareContext>> = const { RefCell::new(None) };
    }

    fn path_for_winrt(path: &Path) -> String {
        let value = path.to_string_lossy().to_string();
        value.strip_prefix(r"\\?\").unwrap_or(&value).to_string()
    }

    fn build_context(hwnd_value: isize) -> Result<ShareContext, String> {
        let hwnd = HWND(hwnd_value as *mut c_void);

        let interop: IDataTransferManagerInterop =
            factory::<DataTransferManager, IDataTransferManagerInterop>()
                .map_err(|error| format!("Windows no pudo iniciar el panel Compartir: {error}"))?;

        let manager: DataTransferManager = unsafe {
            interop
                .GetForWindow(hwnd)
                .map_err(|error| format!("Windows no pudo asociar el panel Compartir a ZGT: {error}"))?
        };

        let current_file = Arc::new(Mutex::new(None::<StorageFile>));
        let file_for_event = Arc::clone(&current_file);

        let revoker = manager
            .DataRequested(move |_sender, args| {
                let Ok(args) = args.ok() else { return; };
                let Ok(request) = args.Request() else { return; };
                let Ok(data) = request.Data() else { return; };

                let file = file_for_event
                    .lock()
                    .ok()
                    .and_then(|guard| guard.clone());

                let Some(file) = file else {
                    let _ = request.FailWithDisplayText(&HSTRING::from(
                        "ZGT no pudo preparar el PDF para compartir."
                    ));
                    return;
                };

                let Ok(item) = file.cast::<IStorageItem>() else {
                    let _ = request.FailWithDisplayText(&HSTRING::from(
                        "Windows no pudo adjuntar el PDF."
                    ));
                    return;
                };

                let items = IIterable::<IStorageItem>::from(vec![item]);

                if let Ok(properties) = data.Properties() {
                    let _ = properties.SetTitle(&HSTRING::from("Compartir PDF de ZGT"));
                    let _ = properties.SetDescription(&HSTRING::from(
                        "Documento generado por ZEOZ Gestión Taller"
                    ));
                }

                let _ = data.SetRequestedOperation(DataPackageOperation::Copy);
                let _ = data.SetStorageItemsReadOnly(&items);
            })
            .map_err(|error| format!("No se pudo preparar el contenido para compartir: {error}"))?;

        Ok(ShareContext {
            hwnd: hwnd_value,
            interop,
            _manager: manager,
            _revoker: revoker,
            current_file,
        })
    }

    fn show_share_sheet(window: &WebviewWindow, path: &Path) -> Result<(), String> {
        let native = window
            .hwnd()
            .map_err(|error| format!("No se pudo obtener la ventana de ZGT: {error}"))?;

        let hwnd_value = native.0 as isize;
        let file_path = path_for_winrt(path);

        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(file_path))
            .map_err(|error| format!("Windows no pudo abrir el PDF temporal: {error}"))?
            .get()
            .map_err(|error| format!("Windows no pudo preparar el PDF para compartir: {error}"))?;

        SHARE_CONTEXT.with(|slot| -> Result<(), String> {
            let mut slot = slot.borrow_mut();

            let needs_rebuild = slot
                .as_ref()
                .map(|context| context.hwnd != hwnd_value)
                .unwrap_or(true);

            if needs_rebuild {
                *slot = Some(build_context(hwnd_value)?);
            }

            let context = slot
                .as_mut()
                .ok_or_else(|| "No se pudo iniciar el panel Compartir.".to_string())?;

            {
                let mut current = context
                    .current_file
                    .lock()
                    .map_err(|_| "No se pudo preparar el PDF para compartir.".to_string())?;
                *current = Some(file);
            }

            let hwnd = HWND(hwnd_value as *mut c_void);
            unsafe {
                context
                    .interop
                    .ShowShareUIForWindow(hwnd)
                    .map_err(|error| format!("Windows no pudo abrir el panel Compartir: {error}"))?;
            }

            Ok(())
        })
    }

    pub async fn share_pdf(
        window: WebviewWindow,
        data_url: String,
        filename: String,
    ) -> Result<(), String> {
        let path = write_temp_pdf(&data_url, &filename)?;
        let window_for_ui = window.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);

        window
            .run_on_main_thread(move || {
                let result = show_share_sheet(&window_for_ui, &path);
                let _ = tx.send(result);
            })
            .map_err(|error| format!("No se pudo abrir el panel Compartir: {error}"))?;

        rx.recv_timeout(std::time::Duration::from_secs(8))
            .map_err(|_| "Windows tardó demasiado en abrir el panel Compartir.".to_string())?
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::*;

    pub async fn share_pdf(
        _window: WebviewWindow,
        _data_url: String,
        _filename: String,
    ) -> Result<(), String> {
        Err("Compartir PDF mediante el panel nativo está disponible en Windows.".to_string())
    }
}

pub async fn share_pdf(
    window: WebviewWindow,
    data_url: String,
    filename: String,
) -> Result<(), String> {
    platform::share_pdf(window, data_url, filename).await
}
