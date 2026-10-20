use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::WebviewWindow;

fn clean_filename(filename: &str) -> String {
    let mut value: String = filename
        .chars()
        .filter(|ch| ch.is_alphanumeric() || matches!(ch, ' ' | '-' | '_' | '.'))
        .take(140)
        .collect();

    value = value.trim().trim_matches('.').to_string();

    if value.is_empty() {
        value = "ZGT-documento.pdf".to_string();
    }

    if !value.to_lowercase().ends_with(".pdf") {
        value.push_str(".pdf");
    }

    value
}

fn decode_pdf_data_url(data_url: &str) -> Result<Vec<u8>, String> {
    let comma = data_url
        .find(',')
        .ok_or_else(|| "El PDF recibido por ZGT no tiene un formato válido.".to_string())?;

    let header = &data_url[..comma];
    if !header.starts_with("data:") || !header.to_ascii_lowercase().contains(";base64") {
        return Err("El PDF recibido por ZGT no está codificado correctamente.".to_string());
    }

    let bytes = STANDARD
        .decode(&data_url[comma + 1..])
        .map_err(|_| "No se pudo decodificar el PDF para compartir.".to_string())?;

    if bytes.len() < 5 || !bytes.windows(4).take(1024).any(|window| window == b"%PDF") {
        return Err("El archivo recibido no parece ser un PDF válido.".to_string());
    }

    Ok(bytes)
}

fn save_temp_pdf(data_url: &str, filename: &str) -> Result<PathBuf, String> {
    let bytes = decode_pdf_data_url(data_url)?;
    let filename = clean_filename(filename);

    let dir = std::env::temp_dir().join("ZGT").join("Compartir");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("No se pudo preparar el PDF para compartir: {error}"))?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let path = dir.join(format!("{stamp}-{filename}"));
    fs::write(&path, bytes)
        .map_err(|error| format!("No se pudo guardar temporalmente el PDF: {error}"))?;

    Ok(path)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{
        ffi::c_void,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex, OnceLock,
        },
    };
    use windows::{
        ApplicationModel::DataTransfer::{DataPackageOperation, DataTransferManager},
        Storage::{IStorageItem, StorageFile},
        Win32::{
            Foundation::HWND,
            System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
        },
    };
    use windows_collections::IIterable;
    use windows_core::{GUID, HRESULT, HSTRING, Interface};

    #[derive(Clone)]
    struct SharePayload {
        path: String,
        filename: String,
    }

    static SHARE_PAYLOAD: OnceLock<Arc<Mutex<Option<SharePayload>>>> = OnceLock::new();
    static SHARE_HANDLER_READY: AtomicBool = AtomicBool::new(false);

    windows_core::imp::define_interface!(
        IDataTransferManagerInterop,
        IDataTransferManagerInterop_Vtbl,
        0x3a3dcd6c_3eab_43dc_bcde_45671ce800c8
    );
    windows_core::imp::interface_hierarchy!(
        IDataTransferManagerInterop,
        windows_core::IUnknown
    );

    #[repr(C)]
    struct IDataTransferManagerInterop_Vtbl {
        base__: windows_core::IUnknown_Vtbl,
        GetForWindow: unsafe extern "system" fn(
            *mut c_void,
            HWND,
            *const GUID,
            *mut *mut c_void,
        ) -> HRESULT,
        ShowShareUIForWindow:
            unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    }

    impl IDataTransferManagerInterop {
        fn get_for_window(&self, hwnd: HWND) -> windows_core::Result<DataTransferManager> {
            unsafe {
                let mut result__: *mut c_void = std::ptr::null_mut();
                (Interface::vtable(self).GetForWindow)(
                    Interface::as_raw(self),
                    hwnd,
                    &DataTransferManager::IID,
                    &mut result__,
                )
                .ok()?;

                if result__.is_null() {
                    return Err(windows_core::Error::from_hresult(HRESULT(
                        0x80004003_u32 as i32,
                    )));
                }

                Ok(DataTransferManager::from_raw(result__))
            }
        }

        fn show_share_ui_for_window(&self, hwnd: HWND) -> windows_core::Result<()> {
            unsafe {
                (Interface::vtable(self).ShowShareUIForWindow)(
                    Interface::as_raw(self),
                    hwnd,
                )
                .ok()
            }
        }
    }

    fn payload_state() -> Arc<Mutex<Option<SharePayload>>> {
        SHARE_PAYLOAD
            .get_or_init(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    fn set_package_file(
        data: windows::ApplicationModel::DataTransfer::DataPackage,
        request: windows::ApplicationModel::DataTransfer::DataRequest,
        deferral: windows::ApplicationModel::DataTransfer::DataRequestDeferral,
        payload: SharePayload,
    ) {
        std::thread::spawn(move || {
            let initialized = unsafe { RoInitialize(RO_INIT_MULTITHREADED).is_ok() };

            let result: Result<(), String> = (|| {
                let path = HSTRING::from(payload.path.clone());
                let file = StorageFile::GetFileFromPathAsync(&path)
                    .map_err(|error| format!("No se pudo abrir el PDF temporal: {error}"))?
                    .join()
                    .map_err(|error| format!("No se pudo preparar el PDF: {error}"))?;

                let item: IStorageItem = file
                    .cast()
                    .map_err(|error| format!("No se pudo preparar el archivo para compartir: {error}"))?;

                let items = IIterable::<IStorageItem>::from(vec![Some(item)]);

                data.SetStorageItems(&items, true)
                    .map_err(|error| format!("Windows no aceptó el PDF para compartir: {error}"))?;

                data.SetRequestedOperation(DataPackageOperation::Copy)
                    .map_err(|error| format!("No se pudo preparar la operación de compartir: {error}"))?;

                Ok(())
            })();

            if let Err(message) = result {
                let _ = request.FailWithDisplayText(&HSTRING::from(message));
            }

            let _ = deferral.Complete();

            if initialized {
                unsafe { RoUninitialize() };
            }
        });
    }

    fn ensure_share_handler(
        interop: &IDataTransferManagerInterop,
        hwnd: HWND,
    ) -> Result<(), String> {
        if SHARE_HANDLER_READY.load(Ordering::Acquire) {
            return Ok(());
        }

        let manager = interop
            .get_for_window(hwnd)
            .map_err(|error| format!("No se pudo iniciar el panel Compartir de Windows: {error}"))?;

        let state = payload_state();

        let _registration_token = manager
            .DataRequested(move |
                _sender: windows_core::Ref<'_, DataTransferManager>,
                args: windows_core::Ref<'_, windows::ApplicationModel::DataTransfer::DataRequestedEventArgs>
            | {
                let payload = match state.lock() {
                    Ok(guard) => guard.clone(),
                    Err(_) => None,
                };

                let Some(payload) = payload else {
                    return;
                };

                let Ok(request) = args.Request() else {
                    return;
                };
                let Ok(data) = request.Data() else {
                    return;
                };

                if let Ok(properties) = data.Properties() {
                    let _ = properties.SetTitle(&HSTRING::from(payload.filename.clone()));
                    let _ = properties.SetDescription(&HSTRING::from(
                        "Documento PDF generado por ZGT",
                    ));
                }

                let Ok(deferral) = request.GetDeferral() else {
                    return;
                };

                set_package_file(data, request, deferral, payload);
            })
            .map_err(|error| format!("No se pudo preparar Compartir de Windows: {error}"))?;

        // Windows conserva el callback mientras la fuente del evento siga viva.
        // El token sólo sería necesario para desregistrarlo explícitamente.
        SHARE_HANDLER_READY.store(true, Ordering::Release);

        Ok(())
    }

    fn share_on_ui_thread(
        window: WebviewWindow,
        path: PathBuf,
        filename: String,
    ) -> Result<(), String> {
        let raw = window
            .window_handle()
            .map_err(|error| format!("No se pudo identificar la ventana de ZGT: {error}"))?
            .as_raw();

        let hwnd = match raw {
            RawWindowHandle::Win32(handle) => HWND(handle.hwnd.get() as *mut c_void),
            _ => return Err("La ventana actual no es una ventana de Windows.".to_string()),
        };

        let state = payload_state();
        {
            let mut guard = state
                .lock()
                .map_err(|_| "No se pudo preparar el PDF para compartir.".to_string())?;
            *guard = Some(SharePayload {
                path: path.to_string_lossy().to_string(),
                filename,
            });
        }

        let interop = windows_core::factory::<DataTransferManager, IDataTransferManagerInterop>()
            .map_err(|error| format!("Windows no pudo iniciar el panel Compartir: {error}"))?;

        ensure_share_handler(&interop, hwnd)?;
        interop
            .show_share_ui_for_window(hwnd)
            .map_err(|error| format!("No se pudo abrir el panel Compartir de Windows: {error}"))
    }

    pub fn share_pdf(
        window: WebviewWindow,
        data_url: String,
        filename: String,
    ) -> Result<(), String> {
        let safe_name = super::clean_filename(&filename);
        let path = super::save_temp_pdf(&data_url, &safe_name)?;

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let ui_window = window.clone();

        window
            .run_on_main_thread(move || {
                let result = share_on_ui_thread(ui_window, path, safe_name);
                let _ = sender.send(result);
            })
            .map_err(|error| format!("No se pudo abrir Compartir de Windows: {error}"))?;

        receiver
            .recv_timeout(std::time::Duration::from_secs(8))
            .map_err(|_| "Windows tardó demasiado en abrir el panel Compartir.".to_string())?
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn share_pdf(
        _window: WebviewWindow,
        _data_url: String,
        _filename: String,
    ) -> Result<(), String> {
        Err("Compartir PDF desde ZGT Desktop está disponible actualmente en Windows.".to_string())
    }
}

pub fn share_pdf(
    window: WebviewWindow,
    data_url: String,
    filename: String,
) -> Result<(), String> {
    platform::share_pdf(window, data_url, filename)
}
