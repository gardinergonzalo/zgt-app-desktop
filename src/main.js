const invoke = window.__TAURI__.core.invoke;

const boot = document.getElementById("boot");
const link = document.getElementById("link");
const bootMessage = document.getElementById("boot-message");
const codeInput = document.getElementById("link-code");
const button = document.getElementById("link-button");
const status = document.getElementById("status");

function formatCode(value) {
  const raw = String(value || "")
    .toUpperCase()
    .replace(/[^A-Z0-9]/g, "")
    .slice(0, 11);

  let result = "";
  for (let i = 0; i < raw.length; i += 1) {
    if (i === 3 || i === 7) result += "-";
    result += raw[i];
  }
  return result;
}

function validCode(value) {
  return /^ZGT-[A-Z0-9]{4}-[A-Z0-9]{4}$/.test(value);
}

function showLink(message = "") {
  boot.classList.remove("screen-active");
  link.classList.add("screen-active");
  status.textContent = message;
  status.classList.remove("ok");
  requestAnimationFrame(() => codeInput.focus());
}

function showBoot(message) {
  link.classList.remove("screen-active");
  boot.classList.add("screen-active");
  bootMessage.textContent = message;
}

function setStatus(message, ok = false) {
  status.textContent = message;
  status.classList.toggle("ok", ok);
}

async function openWorkshop(siteUrl) {
  showBoot("Abriendo tu taller…");
  await invoke("open_workshop", { url: siteUrl });
}

async function refreshSavedWorkshop(saved) {
  showBoot("Actualizando datos del taller…");

  try {
    const resolved = await invoke("resolve_link", { code: saved.code });
    const fresh = {
      code: saved.code,
      workshop_name: resolved.workshop_name,
      site_url: resolved.site_url
    };

    await invoke("save_link", { state: fresh });
    await openWorkshop(fresh.site_url);
  } catch (error) {
    if (saved.site_url) {
      bootMessage.textContent = "Central no respondió. Usando la última vinculación…";
      await openWorkshop(saved.site_url);
      return;
    }
    showLink(String(error || "No se pudo actualizar la vinculación."));
  }
}

async function bootstrap() {
  try {
    const saved = await invoke("load_link");
    if (saved && saved.code) {
      await refreshSavedWorkshop(saved);
      return;
    }
  } catch (error) {
    console.error(error);
  }

  showLink();
}

async function linkWorkshop() {
  const code = formatCode(codeInput.value);
  codeInput.value = code;

  if (!validCode(code)) {
    setStatus("Revisá el formato del código.");
    return;
  }

  button.disabled = true;
  button.textContent = "Vinculando…";
  setStatus("Conectando con ZGT…", true);

  try {
    const resolved = await invoke("resolve_link", { code });
    const state = {
      code,
      workshop_name: resolved.workshop_name,
      site_url: resolved.site_url
    };

    await invoke("save_link", { state });
    setStatus("Taller vinculado", true);
    await openWorkshop(state.site_url);
  } catch (error) {
    setStatus(String(error || "No se pudo vincular el taller."));
    button.disabled = false;
    button.textContent = "Vincular";
  }
}

codeInput.addEventListener("input", () => {
  const formatted = formatCode(codeInput.value);
  if (formatted !== codeInput.value) {
    codeInput.value = formatted;
  }
});

codeInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !button.disabled) {
    linkWorkshop();
  }
});

button.addEventListener("click", linkWorkshop);

bootstrap();
