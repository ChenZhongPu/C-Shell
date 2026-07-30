"use strict";

const container = document.getElementById("terminal");
const status = document.getElementById("status");
const terminal = new Terminal({
  allowProposedApi: false,
  cursorBlink: true,
  cursorStyle: "bar",
  fontFamily:
    'ui-monospace, "Cascadia Code", "SFMono-Regular", Consolas, "Liberation Mono", monospace',
  fontSize: 15,
  scrollback: 5000,
  theme: {
    background: "#101317",
    foreground: "#e6edf3",
    cursor: "#55d6be",
    cursorAccent: "#101317",
    selectionBackground: "#264f78",
  },
});
const fitAddon = new FitAddon.FitAddon();
terminal.loadAddon(fitAddon);
terminal.open(container);
fitAddon.fit();
terminal.focus();

const basePath = window.location.pathname.endsWith("/")
  ? window.location.pathname
  : `${window.location.pathname}/`;
const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
const socket = new WebSocket(
  `${protocol}//${window.location.host}${basePath}ws`,
);
socket.binaryType = "arraybuffer";

const encoder = new TextEncoder();
const sendSize = () => {
  if (socket.readyState === WebSocket.OPEN) {
    socket.send(`resize:${terminal.cols}:${terminal.rows}`);
  }
};

socket.addEventListener("open", () => {
  status.hidden = true;
  sendSize();
  terminal.focus();
});

socket.addEventListener("message", (event) => {
  if (event.data instanceof ArrayBuffer) {
    terminal.write(new Uint8Array(event.data));
  }
});

socket.addEventListener("close", () => {
  status.textContent = document.body.dataset.disconnected;
  status.classList.add("disconnected");
  status.hidden = false;
});

socket.addEventListener("error", () => {
  status.textContent = document.body.dataset.disconnected;
  status.classList.add("disconnected");
  status.hidden = false;
});

terminal.onData((data) => {
  if (socket.readyState === WebSocket.OPEN) {
    socket.send(encoder.encode(data));
  }
});

// Mouse protocols and a few legacy terminal modes use arbitrary 8-bit input
// rather than Unicode text.
terminal.onBinary((data) => {
  if (socket.readyState !== WebSocket.OPEN) {
    return;
  }
  const bytes = new Uint8Array(data.length);
  for (let index = 0; index < data.length; index += 1) {
    bytes[index] = data.charCodeAt(index) & 0xff;
  }
  socket.send(bytes);
});

let resizeFrame;
const resizeObserver = new ResizeObserver(() => {
  cancelAnimationFrame(resizeFrame);
  resizeFrame = requestAnimationFrame(() => {
    fitAddon.fit();
    sendSize();
  });
});
resizeObserver.observe(container);

window.addEventListener("beforeunload", () => {
  socket.close();
});
