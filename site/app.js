const captureTabs = [...document.querySelectorAll("[data-capture]")];
const capturePanel = document.getElementById("capture-panel");
const captureOutput = document.getElementById("tui-capture");
let captureRequest = 0;

async function showCapture(name) {
  if (!["welcome", "session", "working"].includes(name)) return;
  const request = ++captureRequest;
  for (const tab of captureTabs) {
    const active = tab.dataset.capture === name;
    tab.setAttribute("aria-selected", String(active));
    tab.tabIndex = active ? 0 : -1;
    if (active) capturePanel.setAttribute("aria-labelledby", tab.id);
  }
  captureOutput.textContent = "Opening TUI capture…";
  try {
    const response = await fetch(new URL("./captures/" + name + ".txt", document.baseURI));
    if (!response.ok) throw new Error("Capture unavailable");
    const text = await response.text();
    if (request === captureRequest) captureOutput.textContent = text;
  } catch {
    if (request === captureRequest) {
      captureOutput.textContent = "Capture unavailable. See the TUI snapshots in the repository.";
    }
  }
}

captureTabs.forEach((tab, index) => {
  tab.addEventListener("click", () => showCapture(tab.dataset.capture));
  tab.addEventListener("keydown", (event) => {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    const next = event.key === "Home" ? 0 : event.key === "End" ? captureTabs.length - 1
      : (index + (event.key === "ArrowRight" ? 1 : -1) + captureTabs.length) % captureTabs.length;
    captureTabs[next].focus();
    showCapture(captureTabs[next].dataset.capture);
  });
});
showCapture("welcome");

async function copyText(text) {
  if (navigator.clipboard && window.isSecureContext) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const input = document.createElement("textarea");
  input.value = text;
  input.style.position = "fixed";
  input.style.opacity = "0";
  document.body.append(input);
  input.select();
  const copied = document.execCommand("copy");
  input.remove();
  if (!copied) throw new Error("Copy unavailable");
}

const resetTimers = new WeakMap();
document.querySelectorAll("[data-copy]").forEach((button) => {
  button.addEventListener("click", async () => {
    const source = document.getElementById(button.dataset.copy);
    if (!source) return;
    try {
      await copyText(source.textContent.trim());
      button.textContent = "Copied";
    } catch {
      button.textContent = "Select text";
    }
    clearTimeout(resetTimers.get(button));
    resetTimers.set(button, setTimeout(() => { button.textContent = "Copy"; }, 2000));
  });
});
