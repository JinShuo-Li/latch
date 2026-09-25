const captures = new Set(["welcome", "session", "working"]);
const captureTabs = [...document.querySelectorAll("[data-capture]")];
const capturePanel = document.getElementById("capture-panel");
const captureOutput = document.getElementById("tui-capture");
let captureRequest = 0;

async function showCapture(name) {
  if (!captures.has(name)) return;
  const request = ++captureRequest;
  for (const tab of captureTabs) {
    const active = tab.dataset.capture === name;
    tab.setAttribute("aria-selected", String(active));
    tab.tabIndex = active ? 0 : -1;
    if (active) capturePanel.setAttribute("aria-labelledby", tab.id);
  }
  captureOutput.textContent = "Opening TUI capture…";
  try {
    const response = await fetch(new URL(`./captures/${name}.txt`, document.baseURI));
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const text = await response.text();
    if (request === captureRequest) captureOutput.textContent = text;
  } catch {
    if (request === captureRequest) {
      captureOutput.textContent = "Capture unavailable. View the TUI snapshots in the repository.";
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

const examples = {
  interactive: {
    command: "latch",
    description: "Open the terminal UI and ask Latch to inspect, change, or verify code."
  },
  oneoff: {
    command: 'latch -p "Fix the failing test"',
    description: "Run one prompt directly from your shell without opening the TUI."
  },
  resume: {
    command: "latch --resume --latest",
    description: "Resume the newest session for the current workspace."
  }
};
const usageTabs = [...document.querySelectorAll("[data-usage]")];
const usagePanel = document.getElementById("usage-panel");
const usageCode = document.getElementById("usage-code");
const usageDescription = document.getElementById("usage-description");

function showExample(name) {
  const example = examples[name];
  if (!example) return;
  for (const tab of usageTabs) {
    const active = tab.dataset.usage === name;
    tab.setAttribute("aria-selected", String(active));
    tab.tabIndex = active ? 0 : -1;
    if (active) usagePanel.setAttribute("aria-labelledby", tab.id);
  }
  usageCode.textContent = example.command;
  usageDescription.textContent = example.description;
}

usageTabs.forEach((tab, index) => {
  tab.addEventListener("click", () => showExample(tab.dataset.usage));
  tab.addEventListener("keydown", (event) => {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    const next = event.key === "Home" ? 0 : event.key === "End" ? usageTabs.length - 1
      : (index + (event.key === "ArrowRight" ? 1 : -1) + usageTabs.length) % usageTabs.length;
    usageTabs[next].focus();
    showExample(usageTabs[next].dataset.usage);
  });
});

const toast = document.getElementById("copy-toast");
let toastTimer;
function announce(message) {
  toast.textContent = message;
  toast.classList.add("visible");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toast.classList.remove("visible"), 2400);
}

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

document.querySelectorAll("[data-copy]").forEach((button) => {
  button.addEventListener("click", async () => {
    const source = document.getElementById(button.dataset.copy);
    if (!source) return;
    try {
      await copyText(source.textContent.trim());
      announce("Copied to clipboard");
    } catch {
      announce("Copy unavailable — select the command above");
    }
  });
});
