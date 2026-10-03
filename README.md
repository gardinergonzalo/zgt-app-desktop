# ZGT Desktop

Aplicación de escritorio de **ZEOZ Gestión Taller** para Windows y macOS.

## Estado

Versión inicial: **0.1.0**

- Vinculación con código `ZGT-XXXX-XXXX`.
- Central: `https://central.zeoz.com.ar/wp-json/gtc/v1/app/resolve`.
- Guarda localmente código, nombre y URL del taller.
- Al iniciar vuelve a consultar Central y actualiza automáticamente el dominio si cambió.
- Si Central no responde temporalmente, intenta abrir la última URL válida guardada.
- Abre `/wp-admin/` del taller.
- Activa automáticamente “Recuérdame” en el login de WordPress.
- WebView persistente para conservar sesión.
- Windows: instalador NSIS `.exe`.
- macOS: `.dmg` universal para Apple Silicon e Intel.

## Compilación

Los workflows de GitHub Actions generan automáticamente los instaladores de Windows y macOS desde la rama `main`.
