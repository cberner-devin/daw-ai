"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const http = require("node:http");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");
const { once } = require("node:events");

const WebSocketClient = globalThis.WebSocket || require("undici").WebSocket;
const root = path.resolve(__dirname, "..");

class CdpClient {
  static async connect(url) {
    const client = new CdpClient(url);
    await client.ready;
    return client;
  }

  constructor(url) {
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Map();
    this.socket = new WebSocketClient(url);
    this.ready = new Promise((resolve, reject) => {
      this.socket.addEventListener("open", resolve, { once: true });
      this.socket.addEventListener("error", reject, { once: true });
    });
    this.socket.addEventListener("message", (event) => this.handleMessage(event.data));
    this.socket.addEventListener("close", () => {
      for (const { reject } of this.pending.values()) reject(new Error("Chrome DevTools connection closed"));
      this.pending.clear();
    });
  }

  handleMessage(data) {
    const text = typeof data === "string" ? data : Buffer.from(data).toString("utf8");
    const message = JSON.parse(text);
    if (message.id) {
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      if (message.error) pending.reject(new Error(message.error.message));
      else pending.resolve(message.result);
      return;
    }
    for (const listener of this.listeners.get(message.method) || []) listener(message);
  }

  async send(method, params = {}, sessionId = undefined) {
    await this.ready;
    const id = this.nextId;
    this.nextId += 1;
    const message = { id, method, params };
    if (sessionId) message.sessionId = sessionId;
    const result = new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
    this.socket.send(JSON.stringify(message));
    return result;
  }

  on(method, listener) {
    const listeners = this.listeners.get(method) || [];
    listeners.push(listener);
    this.listeners.set(method, listeners);
  }

  close() {
    this.socket.close();
  }
}

function reservePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close((error) => (error ? reject(error) : resolve(port)));
    });
  });
}

function findBrowser() {
  const candidates = [
    process.env.CHROME_PATH,
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
  ].filter(Boolean);
  const playwrightCache = path.join(os.homedir(), ".cache", "ms-playwright");
  if (fs.existsSync(playwrightCache)) findCachedBrowsers(playwrightCache, 0, candidates);
  return candidates.find((candidate) => {
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      return true;
    } catch (_error) {
      return false;
    }
  });
}

function findCachedBrowsers(directory, depth, candidates) {
  if (depth > 3) return;
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const entryPath = path.join(directory, entry.name);
    if (entry.isDirectory()) findCachedBrowsers(entryPath, depth + 1, candidates);
    if (entry.isFile() && (entry.name === "chrome" || entry.name === "headless_shell")) candidates.push(entryPath);
  }
}

async function waitFor(check, description, timeout = 8000) {
  const deadline = Date.now() + timeout;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const value = await check();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await new Promise((resolve) => setTimeout(resolve, 40));
  }
  throw new Error(`Timed out waiting for ${description}${lastError ? `: ${lastError.message}` : ""}`);
}

async function openPage(cdp, url) {
  const { targetId } = await cdp.send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await cdp.send("Target.attachToTarget", { targetId, flatten: true });
  await cdp.send("Runtime.enable", {}, sessionId);
  await cdp.send("Page.enable", {}, sessionId);
  await cdp.send(
    "Emulation.setDeviceMetricsOverride",
    { width: 1600, height: 1000, deviceScaleFactor: 1, mobile: false },
    sessionId,
  );
  await cdp.send("Page.navigate", { url }, sessionId);
  await waitFor(
    async () => evaluate(cdp, sessionId, "document.readyState === 'complete'"),
    `page load for ${url}`,
  );
  return sessionId;
}

async function evaluate(cdp, sessionId, expression) {
  const response = await cdp.send(
    "Runtime.evaluate",
    { expression, awaitPromise: true, returnByValue: true, userGesture: true },
    sessionId,
  );
  if (response.exceptionDetails) {
    const detail = response.exceptionDetails.exception?.description || response.exceptionDetails.text;
    throw new Error(detail);
  }
  return response.result.value;
}

async function mouse(cdp, sessionId, type, x, y, buttons = 0) {
  await cdp.send(
    "Input.dispatchMouseEvent",
    { type, x, y, button: "left", buttons, clickCount: 1 },
    sessionId,
  );
}

async function touch(cdp, sessionId, type, touchPoints) {
  await cdp.send("Input.dispatchTouchEvent", { type, touchPoints }, sessionId);
  await new Promise((resolve) => setTimeout(resolve, 35));
}

async function pressTab(cdp, sessionId) {
  const key = { key: "Tab", code: "Tab", windowsVirtualKeyCode: 9, nativeVirtualKeyCode: 9 };
  await cdp.send("Input.dispatchKeyEvent", { type: "rawKeyDown", ...key }, sessionId);
  await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", ...key }, sessionId);
}

async function pressKey(cdp, sessionId, key, code, virtualKey, modifiers = 0) {
  const values = {
    key,
    code,
    modifiers,
    windowsVirtualKeyCode: virtualKey,
    nativeVirtualKeyCode: virtualKey,
  };
  await cdp.send("Input.dispatchKeyEvent", { type: "rawKeyDown", ...values }, sessionId);
  await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", ...values }, sessionId);
}

async function submitPrompt(cdp, sessionId, prompt, expectedEditCount) {
  await evaluate(cdp, sessionId, `(() => {
    const input = document.querySelector('#prompt-input');
    input.value = ${JSON.stringify(prompt)};
    document.querySelector('#prompt-form').requestSubmit();
  })()`);
  await waitFor(
    async () =>
      evaluate(cdp, sessionId, `document.querySelectorAll('.edit-item').length === ${expectedEditCount}`),
    `prompt: ${prompt}`,
  );
}

async function startAttackerServer(port) {
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { "Content-Type": "text/html" });
    response.end("<!doctype html><title>Untrusted origin</title>");
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", resolve);
  });
  return server;
}

async function terminate(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, "exit");
  child.kill("SIGTERM");
  const timeout = new Promise((resolve) => setTimeout(resolve, 2000, false));
  if ((await Promise.race([exited.then(() => true), timeout])) === false) {
    child.kill("SIGKILL");
    await once(child, "exit");
  }
}

async function closeBrowser(cdp, child) {
  if (!cdp) {
    await terminate(child);
    return;
  }
  await Promise.race([
    cdp.send("Browser.close").catch(() => {}),
    new Promise((resolve) => setTimeout(resolve, 2000)),
  ]);
  cdp.close();
  if (child.exitCode === null && child.signalCode === null) {
    const exited = once(child, "exit").then(() => true);
    const timeout = new Promise((resolve) => setTimeout(resolve, 2000, false));
    if ((await Promise.race([exited, timeout])) === false) await terminate(child);
  }
}

async function removeBrowserProfile(directory) {
  const retryableErrors = new Set(["EACCES", "EBUSY", "ENOTEMPTY", "EPERM"]);
  let lastError;
  for (let attempt = 0; attempt < 80; attempt += 1) {
    try {
      fs.rmSync(directory, { recursive: true, force: true });
      return;
    } catch (error) {
      if (!retryableErrors.has(error.code)) throw error;
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  throw lastError;
}

async function run() {
  const browserPath = findBrowser();
  if (!browserPath) {
    throw new Error("Chrome or Chromium is required. Set CHROME_PATH to its executable.");
  }
  if (process.argv.includes("--check-browser")) {
    console.log(browserPath);
    return;
  }

  const appPort = await reservePort();
  const debugPort = await reservePort();
  const attackerPort = await reservePort();
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), "daw-ai-browser-"));
  const app = spawn(path.join(root, "target", "debug", "daw-ai"), ["--port", String(appPort)], {
    cwd: root,
    env: { ...process.env, DAW_AI_PROMPT_ENGINE: "demo" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const chrome = spawn(
    browserPath,
    [
      "--headless",
      "--no-sandbox",
      "--disable-gpu",
      "--disable-dev-shm-usage",
      "--mute-audio",
      "--autoplay-policy=no-user-gesture-required",
      `--remote-debugging-port=${debugPort}`,
      `--user-data-dir=${profile}`,
      "about:blank",
    ],
    { stdio: ["ignore", "ignore", "pipe"] },
  );
  let attacker;
  let cdp;
  let appErrors = "";
  let chromeErrors = "";
  app.stderr.on("data", (chunk) => {
    appErrors += chunk;
  });
  chrome.stderr.on("data", (chunk) => {
    chromeErrors += chunk;
  });

  try {
    await waitFor(
      async () => fetch(`http://127.0.0.1:${appPort}/api/health`).then((response) => response.ok),
      "Rust server",
    );
    const browserWebSocket = await waitFor(
      async () => {
        const response = await fetch(`http://127.0.0.1:${debugPort}/json/version`);
        if (!response.ok) return false;
        return (await response.json()).webSocketDebuggerUrl;
      },
      "Chrome DevTools endpoint",
      30_000,
    ).catch((error) => {
      const details = chromeErrors.trim();
      if (!details) throw error;
      throw new Error(`${error.message}\nChrome stderr:\n${details}`);
    });
    cdp = await CdpClient.connect(browserWebSocket);
    const appUrl = `http://127.0.0.1:${appPort}`;
    const appSession = await openPage(cdp, appUrl);
    const consoleErrors = [];
    cdp.on("Runtime.consoleAPICalled", (message) => {
      if (message.sessionId === appSession && message.params.type === "error") consoleErrors.push(message.params);
    });
    cdp.on("Runtime.exceptionThrown", (message) => {
      if (message.sessionId === appSession) consoleErrors.push(message.params);
    });

    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.track-row').length === 3"),
      "initial arrangement",
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        "document.querySelector('#advanced-drawer').hidden && document.querySelector('#advanced-drawer').inert",
      ),
      true,
      "closed advanced controls must be inert",
    );

    await cdp.send(
      "Emulation.setDeviceMetricsOverride",
      { width: 390, height: 844, deviceScaleFactor: 1, mobile: true },
      appSession,
    );
    await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 1 }, appSession);
    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "mobile advanced drawer",
    );
    const mobileAdvancedBounds = await evaluate(cdp, appSession, `(() => {
      const drawer = document.querySelector('#advanced-drawer');
      const drawerRect = drawer.getBoundingClientRect();
      const controls = [...drawer.querySelectorAll('.range-with-output, .clip-event')];
      return {
        bodyWidth: document.body.scrollWidth,
        viewportWidth: document.documentElement.clientWidth,
        drawerClientWidth: drawer.clientWidth,
        drawerScrollWidth: drawer.scrollWidth,
        drawerRight: drawerRect.right,
        widestControlRight: Math.max(...controls.map((control) => control.getBoundingClientRect().right)),
      };
    })()`);
    assert.ok(
      mobileAdvancedBounds.bodyWidth <= mobileAdvancedBounds.viewportWidth &&
        mobileAdvancedBounds.drawerScrollWidth <= mobileAdvancedBounds.drawerClientWidth &&
        mobileAdvancedBounds.widestControlRight <= mobileAdvancedBounds.drawerRight,
      `Advanced controls must fit a 390px viewport (${JSON.stringify(mobileAdvancedBounds)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').hidden"),
      "mobile advanced drawer close",
    );
    await evaluate(cdp, appSession, "document.querySelector('#timeline-scroll').scrollLeft = 0");
    const mobileLane = await evaluate(cdp, appSession, `(() => {
      const rect = document.querySelector('.track-lane').getBoundingClientRect();
      return { y: rect.top + rect.height / 2 };
    })()`);
    const selectionBeforePan = await evaluate(
      cdp,
      appSession,
      "document.querySelector('#selection-readout').textContent",
    );
    await evaluate(cdp, appSession, "window.scrollTo(0, 0)");
    await touch(cdp, appSession, "touchStart", [{ x: 250, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchMove", [{ x: 250, y: mobileLane.y - 110 }]);
    await touch(cdp, appSession, "touchMove", [{ x: 250, y: mobileLane.y - 230 }]);
    await touch(cdp, appSession, "touchEnd", []);
    await waitFor(
      async () => evaluate(cdp, appSession, "window.scrollY > 100"),
      "native vertical page panning over a timeline lane",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      selectionBeforePan,
      "vertical panning must not rewrite the selection",
    );
    await evaluate(cdp, appSession, "window.scrollTo(0, 0)");
    await waitFor(async () => evaluate(cdp, appSession, "window.scrollY === 0"), "page scroll reset");
    await touch(cdp, appSession, "touchStart", [{ x: 340, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchMove", [{ x: 250, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchMove", [{ x: 140, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchEnd", []);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#timeline-scroll').scrollLeft > 100"),
      "native mobile timeline panning",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      selectionBeforePan,
      "panning must not rewrite the selection",
    );

    await evaluate(cdp, appSession, `(() => {
      document.querySelector('#timeline-scroll').scrollLeft = 0;
      document.querySelector('#selection-mode-button').click();
    })()`);
    await touch(cdp, appSession, "touchStart", [{ x: 180, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchMove", [{ x: 300, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchEnd", []);
    assert.notEqual(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      selectionBeforePan,
      "explicit mobile selection mode must edit the region",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-mode-button').getAttribute('aria-pressed')"),
      "false",
      "mobile selection mode must return gesture ownership to panning",
    );
    await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: false }, appSession);
    await cdp.send(
      "Emulation.setDeviceMetricsOverride",
      { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false },
      appSession,
    );
    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "desktop advanced drawer",
    );
    const desktopAdvancedBounds = await evaluate(cdp, appSession, `(() => {
      const drawer = document.querySelector('#advanced-drawer');
      const channels = drawer.querySelector('.channel-list');
      const eventLists = [...drawer.querySelectorAll('.clip-event-list')];
      return {
        drawerClientWidth: drawer.clientWidth,
        drawerScrollWidth: drawer.scrollWidth,
        channelsClientWidth: channels.clientWidth,
        channelsScrollWidth: channels.scrollWidth,
        widestEventOverflow: Math.max(
          0,
          ...eventLists.map((list) => list.scrollWidth - list.clientWidth),
        ),
      };
    })()`);
    assert.ok(
      desktopAdvancedBounds.drawerScrollWidth <= desktopAdvancedBounds.drawerClientWidth &&
        desktopAdvancedBounds.channelsScrollWidth <= desktopAdvancedBounds.channelsClientWidth &&
        desktopAdvancedBounds.widestEventOverflow <= 0,
      `Advanced controls must not overflow at 1440x900 (${JSON.stringify(desktopAdvancedBounds)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').hidden"),
      "desktop advanced drawer close",
    );
    await cdp.send(
      "Emulation.setDeviceMetricsOverride",
      { width: 1600, height: 1000, deviceScaleFactor: 1, mobile: false },
      appSession,
    );

    const lane = await evaluate(cdp, appSession, `(() => {
      const rect = document.querySelector('.track-lane').getBoundingClientRect();
      return { left: rect.left, right: rect.right, y: rect.top + rect.height / 2, width: rect.width };
    })()`);
    await mouse(cdp, appSession, "mousePressed", lane.right - 1, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.right - 1, lane.y);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "31.8s - 32.0s",
      "right-edge click must retain a valid selection",
    );
    await mouse(cdp, appSession, "mousePressed", lane.right - 1, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * 0.75, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.75, lane.y);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "24.0s - 32.0s",
      "a backward drag must preserve the true right-edge anchor",
    );

    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * 0.25, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * 0.5, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.5, lane.y);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "8.0s - 16.0s",
    );
    await evaluate(cdp, appSession, "document.querySelector('.track-lane').focus()");
    await pressKey(cdp, appSession, "ArrowRight", "ArrowRight", 39);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "8.3s - 16.3s",
      "keyboard arrows must move the selected region",
    );
    await pressKey(cdp, appSession, "ArrowLeft", "ArrowLeft", 37, 8);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "8.3s - 16.0s",
      "Shift plus Arrow must resize the selected region",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.activeElement.classList.contains('track-lane')"),
      true,
      "keyboard selection must retain timeline focus",
    );
    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * 0.25, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * 0.5, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.5, lane.y);

    await evaluate(cdp, appSession, `(() => {
      const originalClose = AudioContext.prototype.close;
      window.__promptAudioCloseCount = 0;
      AudioContext.prototype.close = function close() {
        window.__promptAudioCloseCount += 1;
        return originalClose.call(this);
      };
      window.__restoreAudioCloseAfterPrompt = () => {
        AudioContext.prototype.close = originalClose;
      };
    })()`);
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#play-button').classList.contains('is-playing')"),
      "playback before prompted edit",
    );
    const initialPromptPlaybackTime = await evaluate(
      cdp,
      appSession,
      "document.querySelector('#current-time').textContent",
    );
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#current-time').textContent !== ${JSON.stringify(initialPromptPlaybackTime)}`,
        ),
      "transport movement before prompted edit",
    );
    const promptSingleFlight = await evaluate(cdp, appSession, `(async () => {
      const originalFetch = window.fetch;
      const deferred = [];
      window.__promptRequestCount = 0;
      window.fetch = function fetch(resource, options) {
        if (resource !== '/api/edits') return originalFetch(resource, options);
        window.__promptRequestCount += 1;
        return new Promise((resolve, reject) => deferred.push({ resource, options, resolve, reject }));
      };
      window.__releasePromptRequests = () => {
        window.fetch = originalFetch;
        for (const request of deferred) {
          originalFetch(request.resource, request.options).then(request.resolve, request.reject);
        }
      };
      const input = document.querySelector('#prompt-input');
      input.value = 'increase volume';
      input.dispatchEvent(new KeyboardEvent('keydown', {
        key: 'Enter', code: 'Enter', ctrlKey: true, bubbles: true, cancelable: true,
      }));
      input.dispatchEvent(new KeyboardEvent('keydown', {
        key: 'Enter', code: 'Enter', metaKey: true, bubbles: true, cancelable: true,
      }));
      await Promise.resolve();
      return {
        requests: window.__promptRequestCount,
        submitDisabled: document.querySelector('#compose-button').disabled,
        transportActive: document.querySelector('#play-button').classList.contains('is-playing'),
      };
    })()`);
    assert.deepEqual(
      promptSingleFlight,
      { requests: 1, submitDisabled: true, transportActive: false },
      "prompt shortcuts must share one in-flight edit request",
    );
    await evaluate(cdp, appSession, "document.querySelector('#prompt-input').value = 'draft the next edit'");
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#play-button').classList.contains('is-playing')"),
      "playback started while prompt is pending",
    );
    await evaluate(cdp, appSession, "window.__releasePromptRequests()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 1"),
      "single-flight prompt completion",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#compose-button').disabled"),
      false,
      "prompt submission must release its lock after completion",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#prompt-input').value"),
      "draft the next edit",
      "a successful request must preserve prompt text drafted while it was pending",
    );
    const promptedEditResumeTime = await evaluate(
      cdp,
      appSession,
      "document.querySelector('#current-time').textContent",
    );
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#play-button').classList.contains('is-playing') &&
            document.querySelector('#current-time').textContent !== ${JSON.stringify(promptedEditResumeTime)}`,
        ),
      "playback restoration after prompted edit",
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__promptAudioCloseCount"),
      2,
      "playback started during a prompt must be rebuilt for the accepted project",
    );
    await evaluate(cdp, appSession, "window.__restoreAudioCloseAfterPrompt()");

    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('#prompt-input');
      input.value = 'make the chords warm and spacious';
      document.querySelector('#prompt-form').requestSubmit();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 2"),
      "compound AI edit",
    );
    const compoundProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.equal(compoundProject.tracks.length, 3, "effect prompt must not add a track");
    const compoundEdit = compoundProject.edits[compoundProject.edits.length - 1];
    assert.equal(compoundEdit.action.type, "compound");
    assert.deepEqual(
      compoundEdit.action.actions.map((action) => action.type),
      ["effect", "filter"],
    );
    const compoundPills = await evaluate(
      cdp,
      appSession,
      "[...document.querySelectorAll('.effect-pill.is-regional')].map((pill) => pill.textContent)",
    );
    assert.equal(compoundPills.some((pill) => /Reverb.*42%/.test(pill)), true);
    assert.equal(
      compoundPills.some((pill) => /Tone filter.*-30%/.test(pill)),
      true,
      "Advanced must expose the filter half of a compound edit",
    );

    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      const deferred = [];
      window.__undoRequestCount = 0;
      window.fetch = function fetch(resource, options) {
        if (resource !== '/api/undo') return originalFetch(resource, options);
        window.__undoRequestCount += 1;
        return new Promise((resolve, reject) => deferred.push({ resource, options, resolve, reject }));
      };
      window.__releaseNextUndoRequest = () => {
        const request = deferred.shift();
        if (!request) return false;
        originalFetch(request.resource, request.options).then(request.resolve, request.reject);
        return true;
      };
      window.__restoreFetchAfterUndo = () => {
        window.fetch = originalFetch;
      };
      const button = document.querySelector('#undo-button');
      button.click();
      button.click();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__undoRequestCount === 1"),
      "first serialized undo request",
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__undoRequestCount"),
      1,
      "a second undo must wait for the first project snapshot",
    );
    await evaluate(cdp, appSession, "window.__releaseNextUndoRequest()");
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__undoRequestCount === 2"),
      "second serialized undo request",
    );
    await evaluate(cdp, appSession, `(() => {
      window.__restoreFetchAfterUndo();
      window.__releaseNextUndoRequest();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 0"),
      "serialized undo completion",
    );

    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () =>
        evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer",
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        "!document.querySelector('#advanced-drawer').hidden && !document.querySelector('#advanced-drawer').inert",
      ),
      true,
    );
    await evaluate(cdp, appSession, `(() => {
      const controls = [...document.querySelector('#advanced-drawer').querySelectorAll('button, input')];
      controls[controls.length - 1].focus();
    })()`);
    await pressTab(cdp, appSession);
    assert.equal(
      await evaluate(cdp, appSession, "document.activeElement.id"),
      "close-advanced",
      "focus must wrap within the modal drawer",
    );
    const summarySpace = await evaluate(cdp, appSession, `(() => {
      const finalClip = [...document.querySelectorAll('.clip-editor')].at(-1);
      finalClip.open = true;
      const summary = finalClip.querySelector('summary');
      summary.focus();
      const allowed = summary.dispatchEvent(
        new KeyboardEvent('keydown', { key: ' ', code: 'Space', bubbles: true, cancelable: true }),
      );
      return {
        allowed,
        focused: document.activeElement === summary,
        playing: document.querySelector('#play-button').classList.contains('is-playing'),
      };
    })()`);
    assert.deepEqual(
      summarySpace,
      { allowed: true, focused: true, playing: false },
      `Space on a clip summary must remain native and transport-neutral (${JSON.stringify(summarySpace)})`,
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const select = document.querySelector('[data-sound-tool="instrument"][data-parameter="waveform"]');
        select.focus();
        const allowed = select.dispatchEvent(
          new KeyboardEvent('keydown', { key: ' ', code: 'Space', bubbles: true, cancelable: true }),
        );
        return {
          allowed,
          playing: document.querySelector('#play-button').classList.contains('is-playing'),
        };
      })()`),
      { allowed: true, playing: false },
      "Space on a select must remain available to the native control",
    );
    await evaluate(cdp, appSession, `(() => {
      const finalClip = [...document.querySelectorAll('.clip-editor')].at(-1);
      finalClip.open = false;
      finalClip.querySelector('summary').focus();
    })()`);
    await pressTab(cdp, appSession);
    assert.equal(
      await evaluate(cdp, appSession, "document.activeElement.id"),
      "close-advanced",
      "collapsed clip controls must not escape the modal focus order",
    );
    await evaluate(cdp, appSession, "[...document.querySelectorAll('.clip-editor')].at(-1).open = true");
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        instruments: document.querySelectorAll('.instrument-tool').length,
        effects: document.querySelectorAll('.effects-tool').length,
        modulators: document.querySelectorAll('.modulator-card').length,
        routes: document.querySelectorAll('.routing-chain').length,
        events: document.querySelectorAll('.clip-event').length,
      })`),
      { instruments: 3, effects: 3, modulators: 3, routes: 3, events: 22 },
      "Advanced must expose every sound tool and the demo clip events",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `fetch('/api/project')
        .then((response) => response.json())
        .then((project) => {
        const chain = document.querySelector('.routing-chain');
        return {
          labels: [...chain.querySelectorAll('i b')].map((label) => label.textContent),
          types: project.tracks[0].routing.edges.map((edge) => edge.type),
        };
      })`),
      { labels: ["MIDI", "AUDIO", "AUDIO"], types: ["midi", "audio", "audio", "control"] },
      "Advanced and project routing must expose compatible edge types",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `[
        ...document.querySelectorAll(
          '[data-parameter="enabled"]:is([data-sound-tool="effect"], [data-sound-tool="modulator"])',
        ),
      ].map((button) => ({ name: button.getAttribute('aria-label'), pressed: button.getAttribute('aria-pressed') }))`),
      [
        { name: "Disable Pulse Kit Punch compressor effect #110", pressed: "true" },
        { name: "Disable Pulse Kit Pulse envelope modulator #150", pressed: "true" },
        { name: "Disable Soft Current Low-pass filter effect #210", pressed: "true" },
        { name: "Disable Soft Current Bass movement modulator #250", pressed: "true" },
        { name: "Disable Glass Chords Chorus effect #310", pressed: "true" },
        { name: "Disable Glass Chords Room effect #311", pressed: "true" },
        { name: "Disable Glass Chords Slow bloom modulator #350", pressed: "true" },
      ],
      "sound-tool toggles must expose contextual names and pressed state",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const controls = [...document.querySelectorAll(
          '[data-sound-tool]:is(input, select, button)',
        )];
        const names = controls.map((control) => control.getAttribute('aria-label'));
        return {
          allNamed: names.every(Boolean),
          allUnique: new Set(names).size === names.length,
          chordMixes: [...document.querySelectorAll(
            '[data-sound-tool="effect"][data-track-id="3"][data-parameter="mix"]',
          )].map((control) => control.getAttribute('aria-label')),
          chordRouting: [...document.querySelectorAll(
            '[data-sound-tool="routing"][data-track-id="3"]',
          )].map((control) => control.getAttribute('aria-label')),
          firstEvent: [...document.querySelectorAll(
            '[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"]',
          )].map((control) => control.getAttribute('aria-label')),
        };
      })()`),
      {
        allNamed: true,
        allUnique: true,
        chordMixes: [
          "Glass Chords Chorus effect #310 mix",
          "Glass Chords Room effect #311 mix",
        ],
        chordRouting: [
          "Move Glass Chords Chorus effect #310 earlier",
          "Move Glass Chords Chorus effect #310 later",
          "Move Glass Chords Room effect #311 earlier",
          "Move Glass Chords Room effect #311 later",
        ],
        firstEvent: [
          "Pulse Kit Pocket beat clip #11 note event #1101 beat",
          "Pulse Kit Pocket beat clip #11 note event #1101 length",
          "Pulse Kit Pocket beat clip #11 note event #1101 pitch",
          "Pulse Kit Pocket beat clip #11 note event #1101 velocity",
        ],
      },
      "every repeated sound-tool control must have a unique contextual name",
    );
    await evaluate(
      cdp,
      appSession,
      "document.querySelector('[data-sound-tool=\"effect\"][data-tool-id=\"210\"][data-parameter=\"enabled\"]').click()",
    );
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return !project.tracks[1].effects[0].enabled;
    }, "disabled low-pass effect");
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const button = document.querySelector(
          '[data-sound-tool="effect"][data-tool-id="210"][data-parameter="enabled"]',
        );
        return {
          name: button.getAttribute('aria-label'),
          pressed: button.getAttribute('aria-pressed'),
          text: button.textContent,
        };
      })()`),
      { name: "Enable Soft Current Low-pass filter effect #210", pressed: "false", text: "Off" },
      "a disabled sound tool must expose its updated action and state",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].effects[0].enabled;
    }, "disabled low-pass effect undo");
    const projectBeforePreciseTools = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      const update = (selector, value) => {
        const control = document.querySelector(selector);
        control.value = value;
        control.dispatchEvent(new Event('change', { bubbles: true }));
      };
      update('[data-sound-tool="instrument"][data-track-id="2"][data-parameter="release"]', '0.025');
      update('[data-sound-tool="effect"][data-track-id="3"][data-tool-id="310"][data-parameter="mix"]', '0.005');
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.version === projectBeforePreciseTools.version + 2 &&
        project.tracks[1].instrument.parameters.release === 0.025 &&
        project.tracks[2].effects[0].parameters.mix === 0.005;
    }, "precise sound-tool values");
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const release = document.querySelector(
          '[data-sound-tool="instrument"][data-track-id="2"][data-parameter="release"]',
        );
        const mix = document.querySelector(
          '[data-sound-tool="effect"][data-track-id="3"][data-tool-id="310"][data-parameter="mix"]',
        );
        return {
          release: { value: release.value, step: release.step, output: release.nextElementSibling.value },
          mix: {
            value: mix.value,
            step: mix.step,
            output: mix.nextElementSibling.value,
            heading: mix.closest('.effect-card').querySelector('.effect-pill b').textContent,
          },
        };
      })()`),
      {
        release: { value: "0.025", step: "any", output: "0.025 s" },
        mix: { value: "0.005", step: "any", output: "0.5%", heading: "0.5%" },
      },
      "authoritative floats must render without range sanitization or display rounding",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[2].effects[0].parameters.mix === 0.28;
    }, "precise effect mix undo");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].instrument.parameters.release === 0.18;
    }, "precise release undo");
    await evaluate(
      cdp,
      appSession,
      "document.querySelector('[data-sound-tool=\"routing\"][data-track-id=\"3\"][data-tool-id=\"311\"][data-sound-value=\"0\"]').click()",
    );
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[2].routing.audio[2] === "effect:311";
    }, "advanced effect reorder");
    assert.equal(
      await evaluate(cdp, appSession, "document.activeElement.dataset.controlKey"),
      "3-routing-311-down",
      "a reordered endpoint effect must focus its remaining enabled direction",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[2].routing.audio[2] === "effect:310";
    }, "advanced effect reorder undo");
    const soundProjectBefore = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      const select = document.querySelector('[data-sound-tool="instrument"][data-track-id="2"][data-parameter="waveform"]');
      select.value = 'sawtooth';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.version === soundProjectBefore.version + 1 && project.tracks[1].instrument.waveform === "sawtooth";
    }, "advanced instrument update");
    assert.equal(
      await evaluate(cdp, appSession, "document.activeElement.dataset.controlKey"),
      "2-instrument-201-waveform",
      "sound-tool updates must restore focus to their control",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].instrument.waveform === "square";
    }, "advanced instrument undo");
    const projectBeforeClipUiMutation = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      const drumClip = document.querySelector('[data-clip-key="1-11"]');
      const bassClip = document.querySelector('[data-clip-key="2-12"]');
      const eventList = drumClip.querySelector('.clip-event-list');
      bassClip.open = false;
      eventList.scrollTop = eventList.scrollHeight;
      window.__clipScrollBeforeMutation = eventList.scrollTop;
      const input = document.querySelector(
        '[data-sound-tool="event"][data-track-id="1"][data-tool-id="1112"][data-parameter="velocity"]',
      );
      input.value = '0.41';
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.version === projectBeforeClipUiMutation.version + 1 &&
        project.tracks[0].clips[0].events.at(-1).velocity === 0.41;
    }, "scrolled clip event mutation");
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const drumClip = document.querySelector('[data-clip-key="1-11"]');
        const eventList = drumClip.querySelector('.clip-event-list');
        const focused = document.activeElement;
        const listRect = eventList.getBoundingClientRect();
        const focusedRect = focused.getBoundingClientRect();
        return {
          collapsed: !document.querySelector('[data-clip-key="2-12"]').open,
          scrollTop: eventList.scrollTop,
          expectedScrollTop: window.__clipScrollBeforeMutation,
          focusKey: focused.dataset.controlKey,
          focusVisible: focusedRect.top >= listRect.top && focusedRect.bottom <= listRect.bottom,
        };
      })()`),
      {
        collapsed: true,
        scrollTop: await evaluate(cdp, appSession, "window.__clipScrollBeforeMutation"),
        expectedScrollTop: await evaluate(cdp, appSession, "window.__clipScrollBeforeMutation"),
        focusKey: "1-clip-11-event-1112-velocity",
        focusVisible: true,
      },
      "clip disclosure, nested scroll, and focused event state must survive authoritative rerenders",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[0].clips[0].events.at(-1).velocity === 0.42;
    }, "scrolled clip event undo");
    await evaluate(cdp, appSession, `(() => {
      document.querySelector('[data-clip-key="2-12"]').open = true;
      document.querySelector('[data-clip-key="1-11"] .clip-event-list').scrollTop = 0;
    })()`);
    const projectBeforeEventReorder = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector(
        '[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"][data-parameter="time"]',
      );
      input.value = '3.999';
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.version === projectBeforeEventReorder.version + 1 &&
        project.tracks[0].clips[0].events.at(-1).id === 1101 &&
        project.tracks[0].clips[0].events.at(-1).time === 3.999;
    }, "event onset reorder");
    const reorderedEventFocus = await evaluate(cdp, appSession, `(() => {
      const eventList = document.querySelector('[data-clip-key="1-11"] .clip-event-list');
      const focused = document.activeElement;
      const listRect = eventList.getBoundingClientRect();
      const focusedRect = focused.getBoundingClientRect();
      return {
        key: focused.dataset.controlKey,
        scrollTop: eventList.scrollTop,
        visible: focusedRect.top >= listRect.top && focusedRect.bottom <= listRect.bottom,
        value: focused.value,
        maximum: focused.max,
        valid: focused.checkValidity(),
      };
    })()`);
    assert.ok(
      reorderedEventFocus.key === "1-clip-11-event-1101-time" &&
        reorderedEventFocus.scrollTop > 0 && reorderedEventFocus.visible &&
        reorderedEventFocus.value === "3.999" && reorderedEventFocus.maximum === "4" &&
        reorderedEventFocus.valid,
      `a reordered event must reveal its restored focus (${JSON.stringify(reorderedEventFocus)})`,
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const originalFetch = window.fetch;
        let requests = 0;
        window.fetch = function fetch(resource, options) {
          if (resource === '/api/sound-tools') requests += 1;
          return originalFetch(resource, options);
        };
        const input = document.querySelector(
          '[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"][data-parameter="time"]',
        );
        input.value = '4';
        input.dispatchEvent(new Event('change', { bubbles: true }));
        window.fetch = originalFetch;
        const restored = document.querySelector(
          '[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"][data-parameter="time"]',
        );
        return { requests, value: restored.value, maximum: restored.max, valid: restored.checkValidity() };
      })()`),
      { requests: 0, value: "3.999", maximum: "4", valid: true },
      "the event-time endpoint must be represented but rejected as an exclusive bound",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[0].clips[0].events[0].id === 1101 && project.tracks[0].clips[0].events[0].time === 0;
    }, "event onset reorder undo");
    await evaluate(
      cdp,
      appSession,
      "document.querySelector('[data-clip-key=\"1-11\"] .clip-event-list').scrollTop = 0",
    );
    const projectBeforeRejectedSoundTool = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const originalFetch = window.fetch;
        let requests = 0;
        window.fetch = function fetch(resource, options) {
          if (resource === '/api/sound-tools') requests += 1;
          return originalFetch(resource, options);
        };
        const input = document.querySelector('[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="pitch"]');
        input.value = '40.5';
        input.dispatchEvent(new Event('change', { bubbles: true }));
        window.fetch = originalFetch;
        const restored = document.querySelector('[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="pitch"]');
        return { requests, value: restored.value, valid: restored.checkValidity() };
      })()`),
      { requests: 0, value: "33", valid: true },
      "fractional MIDI controls must fail client validation without submitting",
    );
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      window.fetch = function fetch(resource, options) {
        if (resource !== '/api/sound-tools') return originalFetch(resource, options);
        window.fetch = originalFetch;
        return Promise.resolve(new Response(JSON.stringify({ error: 'Rejected sound-tool regression' }), {
          status: 400,
          headers: { 'Content-Type': 'application/json' },
        }));
      };
      const input = document.querySelector('[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="pitch"]');
      input.value = '40';
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#toast').textContent === 'Rejected sound-tool regression' &&
            document.querySelector('[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="pitch"]').value === '33'`,
        ),
      "authoritative sound-tool value after rejection",
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        "fetch('/api/project').then((response) => response.json()).then((project) => project.version)",
      ),
      projectBeforeRejectedSoundTool.version,
      "rejected sound-tool edits must not change the project",
    );

    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#play-button').classList.contains('is-playing')"),
      "playback before mixer change",
    );
    const initialMixerPlaybackTime = await evaluate(cdp, appSession, "document.querySelector('#current-time').textContent");
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#current-time').textContent !== ${JSON.stringify(initialMixerPlaybackTime)}`,
        ),
      "transport movement before mixer change",
    );
    const playbackTimeBeforeMix = await evaluate(cdp, appSession, "document.querySelector('#current-time').textContent");
    const projectBeforeMix = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      const deferred = [];
      window.__mixRequestCount = 0;
      window.fetch = function fetch(resource, options) {
        if (resource !== '/api/mix') return originalFetch(resource, options);
        window.__mixRequestCount += 1;
        return new Promise((resolve, reject) => deferred.push({ resource, options, resolve, reject }));
      };
      window.__releaseNextMixRequest = () => {
        const request = deferred.shift();
        if (!request) return false;
        originalFetch(request.resource, request.options).then(request.resolve, request.reject);
        return true;
      };
      window.__restoreFetchAfterMix = () => {
        window.fetch = originalFetch;
      };
    })()`);
    await evaluate(cdp, appSession, "document.querySelector('[data-volume-track]').focus()");
    await pressKey(cdp, appSession, "ArrowRight", "ArrowRight", 39);
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('[data-volume-track="2"]');
      input.value = String(Number(input.value) + 0.01);
      input.dispatchEvent(new Event('input', { bubbles: true }));
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__mixRequestCount === 1"),
      "first serialized mixer request",
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__mixRequestCount"),
      1,
      "a second mixer change must wait for the first response",
    );
    await evaluate(cdp, appSession, "window.__releaseNextMixRequest()");
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__mixRequestCount === 2"),
      "second serialized mixer request",
    );
    await evaluate(cdp, appSession, `(() => {
      window.__restoreFetchAfterMix();
      window.__releaseNextMixRequest();
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return (
        project.version >= projectBeforeMix.version + 2 &&
        Math.abs(project.tracks[0].volume - 0.83) < 0.001 &&
        Math.abs(project.tracks[1].volume - 0.75) < 0.001
      );
    }, "serialized mixer changes");
    assert.equal(
      await evaluate(cdp, appSession, "document.activeElement.dataset.volumeTrack"),
      "2",
      "serialized mixer updates must restore focus to the latest adjusted control",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        first: document.querySelector('[data-volume-track="1"]').value,
        second: document.querySelector('[data-volume-track="2"]').value,
      })`),
      { first: "0.83", second: "0.75" },
      "the final mixer render must include every queued update",
    );
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#play-button').classList.contains('is-playing') &&
            document.querySelector('#current-time').textContent !== ${JSON.stringify(playbackTimeBeforeMix)}`,
        ),
      "playback restoration after mixer change",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#play-button').classList.contains('is-playing')"),
      "playback pause after mixer regression",
    );
    await evaluate(cdp, appSession, `(() => {
      const lane = document.querySelector('.track-lane');
      lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'Home', bubbles: true, cancelable: true }));
      for (let index = 0; index < 32; index += 1) {
        lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true, cancelable: true }));
      }
      const originalFetch = window.fetch;
      const deferredMix = [];
      window.__queuedMixRequestCount = 0;
      window.__queuedPromptBody = null;
      window.fetch = function fetch(resource, options) {
        if (resource === '/api/mix') {
          window.__queuedMixRequestCount += 1;
          return new Promise((resolve, reject) => deferredMix.push({ resource, options, resolve, reject }));
        }
        if (resource === '/api/edits') window.__queuedPromptBody = options.body.toString();
        return originalFetch(resource, options);
      };
      window.__releaseQueuedMix = () => {
        const request = deferredMix.shift();
        originalFetch(request.resource, request.options).then(request.resolve, request.reject);
      };
      window.__restoreFetchAfterQueuedPrompt = () => {
        window.fetch = originalFetch;
      };
      document.querySelector('[data-volume-track="1"]').dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "8.0s - 16.0s",
      "the queued prompt regression must begin with the intended selection",
    );
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__queuedMixRequestCount === 1"),
      "blocking mixer mutation before prompt",
    );
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('#prompt-input');
      input.value = 'increase volume';
      document.querySelector('#prompt-form').requestSubmit();
      document.querySelector('.track-lane').dispatchEvent(
        new KeyboardEvent('keydown', { key: 'Home', bubbles: true, cancelable: true }),
      );
    })()`);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "0.0s - 8.0s",
      "selection must remain editable while the prompt waits in the mutation queue",
    );
    await evaluate(cdp, appSession, "window.__releaseQueuedMix()");
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__queuedPromptBody !== null"),
      "queued prompt request",
    );
    const queuedPromptRange = await evaluate(
      cdp,
      appSession,
      "Object.fromEntries(new URLSearchParams(window.__queuedPromptBody))",
    );
    assert.equal(queuedPromptRange.start, "8", "a queued prompt must retain its submitted start");
    assert.equal(queuedPromptRange.end, "16", "a queued prompt must retain its submitted end");
    await evaluate(cdp, appSession, "window.__restoreFetchAfterQueuedPrompt()");
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "document.querySelectorAll('.edit-item').length === 1 && !document.querySelector('#compose-button').disabled",
        ),
      "queued prompt completion",
    );
    const queuedPromptProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.equal(queuedPromptProject.edits[0].start, 8);
    assert.equal(queuedPromptProject.edits[0].end, 16);
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 0"),
      "queued prompt undo",
    );
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').inert"),
      true,
    );
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').hidden"),
      "drawer to hide",
    );

    await evaluate(cdp, appSession, `(() => {
      window.__audioCloseCount = 0;
      window.__gainNodes = [];
      window.__oscillators = [];
      window.__biquadFilters = [];
      window.__delayNodes = [];
      window.__convolverCount = 0;
      window.__reverbBuffers = [];
      window.__noiseBuffers = [];
      window.__bufferSources = [];
      const originalClose = AudioContext.prototype.close;
      const originalCreateBuffer = AudioContext.prototype.createBuffer;
      const originalCreateBufferSource = AudioContext.prototype.createBufferSource;
      const originalGain = AudioContext.prototype.createGain;
      const originalOscillator = AudioContext.prototype.createOscillator;
      const originalBiquadFilter = AudioContext.prototype.createBiquadFilter;
      const originalOscillatorStart = OscillatorNode.prototype.start;
      const originalOscillatorStop = OscillatorNode.prototype.stop;
      const originalSetValueAtTime = AudioParam.prototype.setValueAtTime;
      const originalExponentialRamp = AudioParam.prototype.exponentialRampToValueAtTime;
      const originalDelay = AudioContext.prototype.createDelay;
      const originalConvolver = AudioContext.prototype.createConvolver;
      const originalSetInterval = window.setInterval.bind(window);
      const originalClearInterval = window.clearInterval.bind(window);
      window.__audioIntervals = new Set();
      window.setInterval = function setInterval(callback, timeout, ...arguments_) {
        const id = originalSetInterval(callback, timeout, ...arguments_);
        if (timeout === 70) window.__audioIntervals.add(id);
        return id;
      };
      window.clearInterval = function clearInterval(id) {
        window.__audioIntervals.delete(id);
        return originalClearInterval(id);
      };
      AudioContext.prototype.createBuffer = function createBuffer(...arguments_) {
        const buffer = originalCreateBuffer.apply(this, arguments_);
        if (arguments_[0] === 2) window.__reverbBuffers.push(buffer);
        if (arguments_[0] === 1) window.__noiseBuffers.push(buffer);
        return buffer;
      };
      AudioContext.prototype.createBufferSource = function createBufferSource(...arguments_) {
        const source = originalCreateBufferSource.apply(this, arguments_);
        window.__bufferSources.push(source);
        return source;
      };
      AudioContext.prototype.close = function close() {
        window.__audioCloseCount += 1;
        return originalClose.call(this);
      };
      AudioContext.prototype.createGain = function createGain(...arguments_) {
        const node = originalGain.apply(this, arguments_);
        window.__gainNodes.push(node);
        return node;
      };
      AudioContext.prototype.createOscillator = function createOscillator(...arguments_) {
        const node = originalOscillator.apply(this, arguments_);
        window.__oscillators.push(node);
        return node;
      };
      AudioContext.prototype.createBiquadFilter = function createBiquadFilter(...arguments_) {
        const node = originalBiquadFilter.apply(this, arguments_);
        window.__biquadFilters.push(node);
        return node;
      };
      AudioParam.prototype.setValueAtTime = function setValueAtTime(value, time) {
        this.dawAiSetValues ||= [];
        this.dawAiSetValues.push({ value, time });
        return originalSetValueAtTime.call(this, value, time);
      };
      AudioParam.prototype.exponentialRampToValueAtTime = function exponentialRampToValueAtTime(value, time) {
        this.dawAiExponentialRamps ||= [];
        this.dawAiExponentialRamps.push({ value, time });
        return originalExponentialRamp.call(this, value, time);
      };
      OscillatorNode.prototype.start = function start(when = 0) {
        this.dawAiStartTime = when;
        return originalOscillatorStart.call(this, when);
      };
      OscillatorNode.prototype.stop = function stop(when = 0) {
        this.dawAiStopTime = when;
        return originalOscillatorStop.call(this, when);
      };
      AudioContext.prototype.createDelay = function createDelay(...arguments_) {
        const node = originalDelay.apply(this, arguments_);
        window.__delayNodes.push(node);
        return node;
      };
      AudioContext.prototype.createConvolver = function createConvolver(...arguments_) {
        window.__convolverCount += 1;
        return originalConvolver.apply(this, arguments_);
      };
    })()`);

    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer for exact event timing",
    );
    await evaluate(cdp, appSession, `(() => {
      const control = document.querySelector(
        '[data-sound-tool="effect"][data-track-id="2"][data-tool-id="210"][data-parameter="mix"]',
      );
      control.value = '0.01';
      control.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].effects[0].parameters.mix === 0.01;
    }, "one-percent low-pass mix");
    await evaluate(cdp, appSession, `(() => {
      document.querySelector('#close-advanced').click();
      window.__biquadFilters = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "window.__biquadFilters.some((node) => node.dawAiAutomation === 'effect-filter' && node.dawAiTrackId === 2 && node.frequency.dawAiSetValues?.length)",
        ),
      "one-percent low-pass playback",
    );
    assert.ok(
      await evaluate(cdp, appSession, `(() => {
        const filter = window.__biquadFilters.find(
          (node) => node.dawAiAutomation === 'effect-filter' && node.dawAiTrackId === 2,
        );
        return filter.frequency.dawAiSetValues.every((entry) => entry.value > 19000 && entry.value < 20000);
      })()`),
      "the first low-pass mix step must remain close to the dry cutoff",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].effects[0].parameters.mix === 0.46;
    }, "one-percent low-pass undo");
    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () =>
        evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer after low-pass playback",
    );
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').hidden"),
      "advanced drawer before bright filter edit",
    );
    await mouse(cdp, appSession, "mousePressed", lane.left, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * (4 / 32), lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * (4 / 32), lane.y);
    await submitPrompt(cdp, appSession, "make the bass bright", 1);
    const brightFilterProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.deepEqual(
      brightFilterProject.edits[0].action,
      { type: "filter", value: 0.3, target: "bass" },
      "the brightness regression must exercise a positive filter offset",
    );
    await evaluate(cdp, appSession, `(() => {
      window.__biquadFilters = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "window.__biquadFilters.some((node) => node.dawAiAutomation === 'tone' && node.dawAiTrackId === 2 && node.frequency.dawAiSetValues?.length > 100)",
        ),
      "positive bass filter automation",
    );
    const brightFilterDirection = await evaluate(cdp, appSession, `(() => {
      const valueAt = (entries, time) => entries.reduce((closest, entry) =>
        Math.abs(entry.time - time) < Math.abs(closest.time - time) ? entry : closest
      ).value;
      const tone = window.__biquadFilters.find(
        (node) => node.dawAiAutomation === 'tone' && node.dawAiTrackId === 2,
      ).frequency.dawAiSetValues;
      const effect = window.__biquadFilters.find(
        (node) => node.dawAiAutomation === 'effect-filter' && node.dawAiTrackId === 2,
      ).frequency.dawAiSetValues;
      const start = Math.min(...tone.map((entry) => entry.time));
      return {
        brightTone: valueAt(tone, start),
        neutralTone: valueAt(tone, start + 4),
        brightEffect: valueAt(effect, start),
        neutralEffect: valueAt(effect, start + 4),
      };
    })()`);
    assert.ok(
      brightFilterDirection.brightTone > brightFilterDirection.neutralTone * 1.25 &&
        Math.abs(brightFilterDirection.brightEffect - brightFilterDirection.neutralEffect) < 0.01,
      `a positive filter edit must open the authoritative tone stage (${JSON.stringify(brightFilterDirection)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 0"),
      "positive bass filter undo",
    );
    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer after positive filter playback",
    );
    await evaluate(cdp, appSession, `(() => {
      const update = (parameter, value) => {
        const input = document.querySelector(
          '[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="' + parameter + '"]',
        );
        input.value = value;
        input.dispatchEvent(new Event('change', { bubbles: true }));
      };
      update('time', '3.9');
      update('pitch', '80');
      update('duration', '4');
      const release = document.querySelector(
        '[data-sound-tool="instrument"][data-track-id="2"][data-parameter="release"]',
      );
      release.value = '5';
      release.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      const event = project.tracks[1].clips[0].events.find((candidate) => candidate.id === 1201);
      return event.time === 3.9 && event.pitch === 80 && event.duration === 4 &&
        project.tracks[1].instrument.parameters.release === 5;
    }, "exact event timing update");
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      window.__oscillators = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "window.__oscillators.some((node) => node.dawAiTrackId === 2 && node.dawAiEventId === 1201)",
        ),
      "exactly timed bass event playback",
    );
    const exactEventPlayback = await evaluate(cdp, appSession, `(() => {
      const oscillator = window.__oscillators.find((node) => node.dawAiTrackId === 2 && node.dawAiEventId === 1201);
      const beatOne = window.__oscillators.find((node) => node.dawAiTrackId === 2 && node.dawAiEventId === 1202);
      const envelope = window.__gainNodes.find((node) => node.dawAiVoiceEnvelope && node.dawAiEventId === 1201);
      const ramps = envelope.gain.dawAiExponentialRamps;
      const peak = ramps.find((entry) => entry.value > 0.001);
      const release = ramps[ramps.length - 1];
      const hold = envelope.gain.dawAiSetValues.find(
        (entry) => entry.time > peak.time && Math.abs(entry.value - peak.value) < 0.000001,
      );
      return {
        offsetFromBeatOne: oscillator.dawAiStartTime - beatOne.dawAiStartTime,
        holdAfterAttack: hold.time - peak.time,
        releaseAfterHold: release.time - hold.time,
        scheduledDuration: oscillator.dawAiStopTime - oscillator.dawAiStartTime - 0.03,
      };
    })()`);
    const beatDuration = 60 / 112;
    assert.ok(
      Math.abs(exactEventPlayback.offsetFromBeatOne - 2.9 * beatDuration) < 0.01,
      `event beat 3.9 must retain its exact offset from beat 1 (${JSON.stringify(exactEventPlayback)})`,
    );
    assert.ok(
      Math.abs(exactEventPlayback.holdAfterAttack - (4 * beatDuration - 0.008)) < 0.01 &&
        Math.abs(exactEventPlayback.releaseAfterHold - 5) < 0.01 &&
        Math.abs(exactEventPlayback.scheduledDuration - (4 * beatDuration + 5)) < 0.01,
      `pitched envelopes must honor the full configured release (${JSON.stringify(exactEventPlayback)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    for (const [condition, label] of [
      ["project.tracks[1].instrument.parameters.release === 0.18", "exact event release undo"],
      ["event.duration === 0.7", "exact event duration undo"],
      ["event.pitch === 33", "exact event pitch undo"],
      ["event.time === 0", "exact event timing undo"],
    ]) {
      await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
      await waitFor(
        async () =>
          evaluate(
            cdp,
            appSession,
            `fetch('/api/project').then((response) => response.json()).then((project) => {
              const event = project.tracks[1].clips[0].events.find((candidate) => candidate.id === 1201);
              return ${condition};
            })`,
          ),
        label,
      );
    }

    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer for attack chase",
    );
    await evaluate(cdp, appSession, `(() => {
      const attack = document.querySelector(
        '[data-sound-tool="instrument"][data-track-id="2"][data-parameter="attack"]',
      );
      attack.value = '2';
      attack.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].clips[0].events[0].duration === 0.7 &&
        project.tracks[1].instrument.parameters.attack === 2;
    }, "short note with long attack configuration");
    await evaluate(cdp, appSession, `(() => {
      document.querySelector('#close-advanced').click();
      window.__gainNodes = [];
      window.__oscillators = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "window.__oscillators.some((node) => node.dawAiTrackId === 2 && node.dawAiEventId === 1201)",
        ),
      "short bass note with long attack",
    );
    const shortLongAttack = await evaluate(cdp, appSession, `(() => {
      const oscillator = window.__oscillators.find(
        (node) => node.dawAiTrackId === 2 && node.dawAiEventId === 1201,
      );
      const envelope = window.__gainNodes.find(
        (node) => node.dawAiVoiceEnvelope && node.dawAiTrackId === 2 && node.dawAiEventId === 1201,
      );
      const [noteOff, release] = envelope.gain.dawAiExponentialRamps;
      return {
        attack: oscillator.dawAiInstrumentAttack,
        noteDuration: noteOff.time - oscillator.dawAiStartTime,
        noteOffLevel: noteOff.value,
        releaseDuration: release.time - noteOff.time,
      };
    })()`);
    assert.ok(
      shortLongAttack.attack === 2 &&
        Math.abs(shortLongAttack.noteDuration - 0.7 * beatDuration) < 0.01 &&
        shortLongAttack.noteOffLevel > 0.0001 && shortLongAttack.noteOffLevel < 0.001 &&
        Math.abs(shortLongAttack.releaseDuration - 0.18) < 0.01,
      `a short note must release from its partial attack level (${JSON.stringify(shortLongAttack)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer for attack chase duration",
    );
    await evaluate(cdp, appSession, `(() => {
      const duration = document.querySelector(
        '[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="duration"]',
      );
      duration.value = '4';
      duration.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].clips[0].events[0].duration === 4 &&
        project.tracks[1].instrument.parameters.attack === 2;
    }, "long attack chase configuration");
    await evaluate(cdp, appSession, `(() => {
      document.querySelector('#close-advanced').click();
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "Number(document.querySelector('#current-time').textContent.split(':')[1]) >= 0.3",
        ),
      "playback inside the configured attack",
    );
    await evaluate(cdp, appSession, `(() => {
      document.querySelector('#play-button').click();
      window.__gainNodes = [];
      window.__oscillators = [];
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "window.__oscillators.some((node) => node.dawAiTrackId === 2 && node.dawAiEventId === 1201 && node.dawAiChased)",
        ),
      "bass voice chased during attack",
    );
    const chasedAttack = await evaluate(cdp, appSession, `(() => {
      const envelope = window.__gainNodes.find(
        (node) => node.dawAiVoiceEnvelope && node.dawAiTrackId === 2 && node.dawAiEventId === 1201,
      );
      return {
        current: envelope.gain.dawAiSetValues[0].value,
        peak: Math.max(...envelope.gain.dawAiExponentialRamps.map((entry) => entry.value)),
      };
    })()`);
    assert.ok(
      chasedAttack.current > 0.0001 && chasedAttack.current < chasedAttack.peak * 0.5,
      `a chased attack must resume below its eventual peak (${JSON.stringify(chasedAttack)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    for (const [condition, label] of [
      ["project.tracks[1].clips[0].events[0].duration === 0.7", "attack chase duration undo"],
      ["project.tracks[1].instrument.parameters.attack === 0.008", "attack chase undo"],
    ]) {
      await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
      await waitFor(
        async () =>
          evaluate(
            cdp,
            appSession,
            `fetch('/api/project').then((response) => response.json()).then((project) => ${condition})`,
          ),
        label,
      );
    }

    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer for drum controls",
    );
    const beforeDrumTools = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      const update = (selector, value) => {
        const control = document.querySelector(selector);
        control.value = value;
        control.dispatchEvent(new Event('change', { bubbles: true }));
      };
      update('[data-sound-tool="instrument"][data-track-id="1"][data-parameter="waveform"]', 'sawtooth');
      update('[data-sound-tool="instrument"][data-track-id="1"][data-parameter="attack"]', '2');
      update('[data-sound-tool="instrument"][data-track-id="1"][data-parameter="release"]', '5');
      update('[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"][data-parameter="duration"]', '0.0625');
      update('[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"][data-parameter="pitch"]', '48');
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      const drums = project.tracks[0];
      const event = drums.clips[0].events[0];
      return project.version === beforeDrumTools.version + 5 && drums.instrument.waveform === "sawtooth" &&
        drums.instrument.parameters.attack === 2 && drums.instrument.parameters.release === 5 &&
        event.duration === 0.0625 && event.pitch === 48;
    }, "drum sound-tool updates");
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const input = document.querySelector('[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"][data-parameter="duration"]');
        return { value: input.value, valid: input.checkValidity() };
      })()`),
      { value: "0.0625", valid: true },
      "a sixteenth-of-a-beat duration must round-trip as a valid control value",
    );
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"][data-parameter="duration"]');
      input.value = '4';
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.version === beforeDrumTools.version + 6 && project.tracks[0].clips[0].events[0].duration === 4;
    }, "maximum drum duration update");
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      window.__oscillators = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "window.__oscillators.some((node) => node.dawAiTrackId === 1 && node.dawAiEventPitch === 48)",
        ),
      "configured drum event playback",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const node = window.__oscillators.find((candidate) => candidate.dawAiTrackId === 1 && candidate.dawAiEventPitch === 48);
        return {
          type: node.type,
          duration: node.dawAiEventDuration,
          attack: node.dawAiInstrumentAttack,
          release: node.dawAiInstrumentRelease,
        };
      })()`),
      { type: "sawtooth", duration: 4, attack: 2, release: 5 },
      "drum playback must consume event and instrument parameters",
    );
    assert.ok(
      await evaluate(cdp, appSession, `(() => {
        const node = window.__oscillators.find(
          (candidate) => candidate.dawAiTrackId === 1 && candidate.dawAiEventPitch === 48,
        );
        const envelope = window.__gainNodes.find(
          (candidate) => candidate.dawAiVoiceEnvelope && candidate.dawAiVoiceKind === 'tonal' &&
            candidate.dawAiTrackId === 1 && candidate.dawAiEventId === 1101,
        );
        const attackRamp = envelope.gain.dawAiExponentialRamps[0];
        return Math.abs((attackRamp.time - node.dawAiStartTime) - 2) < 0.01;
      })()`),
      "drum playback must retain the configured attack duration",
    );
    assert.ok(
      await evaluate(cdp, appSession, `(() => {
        const node = window.__oscillators.find((candidate) => candidate.dawAiTrackId === 1 && candidate.dawAiEventPitch === 48);
        return Math.abs((node.dawAiStopTime - node.dawAiStartTime - 0.01) - (4 * 60 / 112 + 5)) < 0.01;
      })()`),
      "drum playback must honor the full configured release",
    );
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "Number(document.querySelector('#current-time').textContent.split(':')[1]) >= 0.1",
        ),
      "playback inside the long drum voices",
    );
    await evaluate(cdp, appSession, `(() => {
      document.querySelector('#play-button').click();
      window.__oscillators = [];
      window.__bufferSources = [];
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `window.__oscillators.some(
            (node) => node.dawAiTrackId === 1 && node.dawAiEventPitch === 48 && node.dawAiChased,
          ) && window.__bufferSources.some((source) => source.dawAiTrackId === 1 && source.dawAiChased)`,
        ),
      "long drum voices chased after resume",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    for (const [condition, label] of [
      ["project.tracks[0].clips[0].events[0].duration === 0.0625", "maximum drum duration undo"],
      ["project.tracks[0].clips[0].events[0].pitch === 36", "drum pitch undo"],
      ["project.tracks[0].clips[0].events[0].duration === 0.25", "drum duration undo"],
      ["project.tracks[0].instrument.parameters.release === 0.18", "drum release undo"],
      ["project.tracks[0].instrument.parameters.attack === 0.002", "drum attack undo"],
      ["project.tracks[0].instrument.waveform === 'sine'", "drum waveform undo"],
    ]) {
      await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
      await waitFor(
        async () =>
          evaluate(
            cdp,
            appSession,
            `fetch('/api/project').then((response) => response.json()).then((project) => ${condition})`,
          ),
        label,
      );
    }

    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer for modulation rate",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(async () => {
        const currentProject = await fetch('/api/project').then((response) => response.json());
        const projectTrack = currentProject.tracks.find((track) => track.id === 2);
        const control = document.querySelector('[data-sound-tool="modulator"][data-track-id="2"][data-parameter="target"]');
        return {
          published: projectTrack.modulationTargets.map((target) => target.id),
          rendered: [...control.options].map((option) => option.value),
        };
      })()`),
      {
        published: [
          "instrument.attack",
          "instrument.release",
          "instrument.tone",
          "instrument.pitch",
          "track.volume",
          "effect:210.mix",
        ],
        rendered: [
          "instrument.attack",
          "instrument.release",
          "instrument.tone",
          "instrument.pitch",
          "track.volume",
          "effect:210.mix",
        ],
      },
      "Advanced must render the backend's complete modulation-target contract",
    );
    await evaluate(cdp, appSession, `(() => {
      const control = document.querySelector('[data-sound-tool="modulator"][data-track-id="2"][data-parameter="rate"]');
      control.value = '20';
      control.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].modulators[0].parameters.rate === 20;
    }, "20 Hz tone modulation update");
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await evaluate(cdp, appSession, `(() => {
      window.__biquadFilters = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "window.__biquadFilters.some((node) => node.dawAiAutomation === 'tone' && node.dawAiTrackId === 2 && node.frequency.dawAiSetValues?.length > 100)",
        ),
      "high-rate tone modulation automation",
    );
    const toneAutomation = await evaluate(cdp, appSession, `(() => {
      const node = window.__biquadFilters.find((candidate) => candidate.dawAiAutomation === 'tone' && candidate.dawAiTrackId === 2);
      const times = [...new Set(node.frequency.dawAiSetValues.map((entry) => entry.time))].sort((left, right) => left - right);
      return {
        gap: Math.max(...times.slice(1).map((time, index) => time - times[index])),
        lowPasses: window.__biquadFilters.filter((candidate) => candidate.type === 'lowpass').length,
      };
    })()`);
    assert.ok(
      toneAutomation.gap <= 0.0063 && toneAutomation.lowPasses === 6,
      `tone modulation must use only the six authoritative track stages (${JSON.stringify(toneAutomation)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");

    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer for pitch modulation",
    );
    await evaluate(cdp, appSession, `(() => {
      const control = document.querySelector('[data-sound-tool="modulator"][data-track-id="2"][data-parameter="target"]');
      control.value = 'instrument.pitch';
      control.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].modulators[0].target === "instrument.pitch";
    }, "pitch modulation target update");
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await evaluate(cdp, appSession, `(() => {
      window.__oscillators = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "window.__oscillators.some((node) => node.dawAiTrackId === 2 && node.detune.dawAiSetValues?.length > 20)",
        ),
      "continuous pitch modulation automation",
    );
    const pitchAutomation = await evaluate(cdp, appSession, `(() => {
      const node = window.__oscillators.find((candidate) => candidate.dawAiTrackId === 2 && candidate.detune.dawAiSetValues?.length > 20);
      const values = node.detune.dawAiSetValues;
      return { count: values.length, spread: Math.max(...values.map((entry) => entry.value)) - Math.min(...values.map((entry) => entry.value)) };
    })()`);
    assert.ok(pitchAutomation.count > 20 && pitchAutomation.spread > 1, "vibrato must vary continuously within a note");
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");

    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer for envelope modulation",
    );
    await evaluate(cdp, appSession, `(() => {
      const update = (parameter, value) => {
        const control = document.querySelector('[data-sound-tool="modulator"][data-track-id="2"][data-parameter="' + parameter + '"]');
        control.value = value;
        control.dispatchEvent(new Event('change', { bubbles: true }));
      };
      update('shape', 'square');
      update('target', 'instrument.attack');
    })()`);
    await waitFor(async () => {
      const current = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return current.tracks[1].modulators[0].shape === "square" &&
        current.tracks[1].modulators[0].target === "instrument.attack";
    }, "attack modulation routing");
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await evaluate(cdp, appSession, `(() => {
      window.__oscillators = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "window.__oscillators.some((node) => node.dawAiTrackId === 2 && node.dawAiEventId === 1201)",
        ),
      "attack-modulated note",
    );
    assert.ok(
      await evaluate(
        cdp,
        appSession,
        "window.__oscillators.find((node) => node.dawAiTrackId === 2 && node.dawAiEventId === 1201).dawAiInstrumentAttack > 0.09",
      ),
      "instrument attack modulation must reach note playback",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");

    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer for release modulation",
    );
    await evaluate(cdp, appSession, `(() => {
      const control = document.querySelector('[data-sound-tool="modulator"][data-track-id="2"][data-parameter="target"]');
      control.value = 'instrument.release';
      control.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const current = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return current.tracks[1].modulators[0].target === "instrument.release";
    }, "release modulation routing");
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await evaluate(cdp, appSession, `(() => {
      window.__oscillators = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "window.__oscillators.some((node) => node.dawAiTrackId === 2 && node.dawAiEventId === 1201)",
        ),
      "release-modulated note",
    );
    assert.ok(
      await evaluate(
        cdp,
        appSession,
        "window.__oscillators.find((node) => node.dawAiTrackId === 2 && node.dawAiEventId === 1201).dawAiInstrumentRelease > 0.5",
      ),
      "instrument release modulation must reach note playback",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");

    for (const [target, label] of [
      ["track.volume", "volume modulation routing"],
      ["effect:210.mix", "effect modulation routing"],
    ]) {
      await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
      await waitFor(
        async () =>
          evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
        `advanced drawer for ${label}`,
      );
      await evaluate(cdp, appSession, `(() => {
        const control = document.querySelector('[data-sound-tool="modulator"][data-track-id="2"][data-parameter="target"]');
        control.value = ${JSON.stringify(target)};
        control.dispatchEvent(new Event('change', { bubbles: true }));
      })()`);
      await waitFor(async () => {
        const current = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
        return current.tracks[1].modulators[0].target === target;
      }, label);
      await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
      await evaluate(cdp, appSession, `(() => {
        window.__gainNodes = [];
        window.__biquadFilters = [];
        document.querySelector('#rewind-button').click();
        document.querySelector('#play-button').click();
      })()`);
      if (target === "track.volume") {
        await waitFor(
          async () =>
            evaluate(
              cdp,
              appSession,
              "window.__gainNodes.some((node) => node.dawAiAutomation === 'level' && node.dawAiTrackId === 2 && node.gain.dawAiSetValues?.length)",
            ),
          "volume-modulated track automation",
        );
        assert.ok(
          await evaluate(cdp, appSession, `(() => {
            const gate = window.__gainNodes.find((node) => node.dawAiAutomation === 'level' && node.dawAiTrackId === 2);
            return gate.gain.dawAiSetValues[0].value > 0.85;
          })()`),
          "track volume modulation must reach channel automation",
        );
      } else {
        await waitFor(
          async () =>
            evaluate(
              cdp,
              appSession,
              "window.__biquadFilters.some((node) => node.dawAiAutomation === 'effect-filter' && node.dawAiTrackId === 2 && node.frequency.dawAiSetValues?.length)",
            ),
          "effect-modulated filter automation",
        );
        assert.ok(
          await evaluate(cdp, appSession, `(() => {
            const filter = window.__biquadFilters.find((node) => node.dawAiAutomation === 'effect-filter' && node.dawAiTrackId === 2);
            const values = filter.frequency.dawAiSetValues.map((entry) => entry.value);
            return Math.max(...values) < 10000 && Math.max(...values) - Math.min(...values) > 1000;
          })()`),
          "effect mix modulation must reach effect playback",
        );
      }
      await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    }

    for (const [condition, label] of [
      ["modulator.target === 'track.volume'", "effect target undo"],
      ["modulator.target === 'instrument.release'", "volume target undo"],
      ["modulator.target === 'instrument.attack'", "release target undo"],
      ["modulator.target === 'instrument.pitch'", "attack target undo"],
      ["modulator.shape === 'sine'", "modulator shape undo"],
      ["modulator.target === 'instrument.tone'", "pitch target undo"],
      ["modulator.parameters.rate === 0.25", "modulation rate undo"],
    ]) {
      await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
      await waitFor(
        async () =>
          evaluate(
            cdp,
            appSession,
            `fetch('/api/project').then((response) => response.json()).then((current) => {
              const modulator = current.tracks[1].modulators[0];
              return ${condition};
            })`,
          ),
        label,
      );
    }
    await evaluate(cdp, appSession, `(() => {
      window.__audioCloseCount = 0;
      window.__reverbBuffers = [];
      window.__noiseBuffers = [];
    })()`);

    const delayedStart = await evaluate(cdp, appSession, `(async () => {
      const originalResume = AudioContext.prototype.resume;
      const resumeResolvers = [];
      AudioContext.prototype.resume = function resume() {
        return new Promise((resolve) => resumeResolvers.push(resolve));
      };
      const button = document.querySelector('#play-button');
      button.click();
      const startClaimedTransport = button.classList.contains('is-playing');
      button.click();
      const pendingStarts = resumeResolvers.length;
      resumeResolvers.forEach((resolve) => resolve());
      await Promise.resolve();
      await new Promise((resolve) => setTimeout(resolve, 0));
      AudioContext.prototype.resume = originalResume;
      const result = {
        startClaimedTransport,
        pendingStarts,
        activeIntervals: window.__audioIntervals.size,
        transportActive: button.classList.contains('is-playing'),
        closedContexts: window.__audioCloseCount,
      };
      window.__audioCloseCount = 0;
      return result;
    })()`);
    assert.deepEqual(
      delayedStart,
      {
        startClaimedTransport: true,
        pendingStarts: 1,
        activeIntervals: 0,
        transportActive: false,
        closedContexts: 1,
      },
      "a second toggle must cancel the single pending start without leaking a scheduler",
    );

    const initialChordId = compoundProject.tracks.find((track) => track.role === "chords").id;
    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * 0.25, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.25, lane.y);
    await evaluate(cdp, appSession, `(() => {
      window.__oscillators = [];
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `window.__oscillators.some((node) => node.dawAiTrackId === ${initialChordId} && node.dawAiChased)`,
        ),
      "sustained chord chase at the playback start",
    );
    const seededBuffers = await evaluate(cdp, appSession, `({
      reverb: window.__reverbBuffers.map((buffer) => Array.from(buffer.getChannelData(0).slice(0, 128))),
      noise: window.__noiseBuffers.map((buffer) => Array.from(buffer.getChannelData(0).slice(0, 128))),
    })`);
    assert.equal(seededBuffers.reverb.length, 2, "each playback context must create one reverb impulse");
    assert.equal(seededBuffers.noise.length, 2, "each playback context must create one noise buffer");
    assert.deepEqual(
      seededBuffers.reverb[0],
      seededBuffers.reverb[1],
      "reverb impulses must be stable across playback contexts",
    );
    assert.deepEqual(
      seededBuffers.noise[0],
      seededBuffers.noise[1],
      "synthesized noise must be stable across playback contexts",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#play-button').classList.contains('is-playing')"),
      "pause before genre composition workflow",
    );
    const audioCloseCountBeforeGenreEdit = await evaluate(cdp, appSession, "window.__audioCloseCount");

    await mouse(cdp, appSession, "mousePressed", lane.left, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * (4 / 32), lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * (4 / 32), lane.y);
    await submitPrompt(cdp, appSession, "add a sawtooth lead", 1);
    const sawtoothLeadProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.deepEqual(
      sawtoothLeadProject.edits[0].action,
      {
        type: "compound",
        actions: [
          { type: "add-track", target: "lead" },
          { type: "instrument", name: "sawtooth", value: 0, target: "lead" },
        ],
      },
      "a waveform-qualified add prompt must create and then configure the requested track",
    );
    assert.equal(
      sawtoothLeadProject.tracks.find((track) => track.role === "lead").instrument.waveform,
      "sawtooth",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "fetch('/api/project').then((response) => response.json()).then((project) => project.edits.length === 0 && !project.tracks.some((track) => track.role === 'lead'))",
        ),
      "sawtooth lead prompt undo",
    );
    await submitPrompt(cdp, appSession, "turn this section into a dubstep drop", 1);
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#compose-button').disabled"),
      "first genre composition completion",
    );
    const genreProject = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
    const genreActions = genreProject.edits[0].action.actions;
    assert.deepEqual(
      genreActions.map((action) => action.type),
      ["midi-clip", "midi-clip", "instrument", "configure", "configure", "configure", "effect", "gain"],
      "a genre request must be built from generic sound-graph operations",
    );
    assert.equal(genreProject.tracks.length, 3, "composing a dubstep section must not inject a canned lead track");
    const genreDrums = genreProject.tracks.find((track) => track.role === "drums");
    const genreBass = genreProject.tracks.find((track) => track.role === "bass");
    const drumMidi = genreDrums.clips.find((clip) => clip.label === "Half-time drums");
    const bassMidi = genreBass.clips.find((clip) => clip.label === "Syncopated bass");
    assert.deepEqual(
      [drumMidi.start, drumMidi.end, bassMidi.start, bassMidi.end],
      [0, 4, 0, 4],
      "the authored MIDI clips must cover the selected section",
    );
    assert.ok(
      drumMidi.events.every((event) => event.type === "note") &&
        drumMidi.events.some((event) => event.pitch === 36) &&
        drumMidi.events.some((event) => event.pitch === 38) &&
        drumMidi.events.some((event) => event.pitch === 41) &&
        drumMidi.events.some((event) => event.pitch === 49),
      "drums must be represented as explicit General MIDI notes",
    );
    assert.deepEqual(
      bassMidi.events.map((event) => event.pitch),
      [29, 29, 32, 29, 27, 29, 36],
      "the bass MIDI must contain the planned low syncopated phrase",
    );
    const wobble = genreBass.modulators.at(-1);
    assert.deepEqual(
      [genreBass.instrument.waveform, wobble.shape, wobble.parameters.rate, wobble.parameters.depth, wobble.target],
      ["sawtooth", "square", 2, 0.72, "instrument.tone"],
      "the bass instrument and modulation must realize the sound-design portion of the plan",
    );
    assert.equal(genreBass.modulators.length, 1, "genre composition must reuse the bass modulator");

    await submitPrompt(cdp, appSession, "make the drop hit harder", 2);
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#compose-button').disabled"),
      "genre refinement completion",
    );
    const refinedGenre = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
    assert.equal(refinedGenre.tracks.length, 3, "refining a genre section must reuse the sound graph");
    assert.equal(
      refinedGenre.tracks.find((track) => track.role === "bass").modulators.length,
      1,
      "refining a genre section must not stack identical bass modulators",
    );
    assert.equal(
      refinedGenre.tracks
        .find((track) => track.role === "bass")
        .clips.filter((clip) => clip.label === "Syncopated bass").length,
      1,
      "refining the same section must replace, not duplicate, its bass MIDI",
    );
    const refinedBassOnsetId = refinedGenre.tracks
      .find((track) => track.role === "bass")
      .clips.find((clip) => clip.label === "Syncopated bass")
      .events[0].id;

    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      window.__oscillators = [];
      window.__bufferSources = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#play-button').classList.contains('is-playing')"),
      "generic MIDI composition playback",
    );
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `window.__oscillators.some(
            (node) => node.dawAiTrackId === ${genreBass.id} && node.dawAiEventId === ${refinedBassOnsetId},
          ) && window.__oscillators.some(
            (node) => node.dawAiTrackId === ${genreDrums.id} && node.dawAiEventPitch === 36,
          ) && window.__oscillators.some(
            (node) => node.dawAiTrackId === ${genreDrums.id} && node.dawAiDrumType === "tom",
          ) && window.__oscillators.some(
            (node) => node.dawAiTrackId === ${genreDrums.id} && node.dawAiDrumType === "cymbal",
          )`,
        ),
      "authored bass and grouped General MIDI drum playback",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 1"),
      "genre refinement undo",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "document.querySelectorAll('.edit-item').length === 0 && document.querySelectorAll('.track-row').length === 3",
        ),
      "genre composition undo",
    );
    await evaluate(cdp, appSession, `window.__audioCloseCount = ${audioCloseCountBeforeGenreEdit}`);

    await mouse(cdp, appSession, "mousePressed", lane.left, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * (4 / 32), lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * (4 / 32), lane.y);
    await submitPrompt(cdp, appSession, "add a lead", 1);
    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * (1 / 32), lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * (3 / 32), lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * (3 / 32), lane.y);
    await submitPrompt(cdp, appSession, "rewrite the lead MIDI clip", 2);
    const splitClipProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    const splitLead = splitClipProject.tracks.find((track) => track.role === "lead");
    const retainedSplitClips = splitLead.clips.filter((clip) => clip.label === "AI variation");
    assert.equal(
      retainedSplitClips.length,
      2,
      `inner region replacement must retain both clip sides (${JSON.stringify(splitLead.clips)})`,
    );
    const [splitLeft, splitRight] = retainedSplitClips;
    const replacementMidiClip = splitLead.clips.find((clip) => clip.label === "AI MIDI clip");
    const precedingClipEvent = replacementMidiClip.events.find((event) => event.time === 3);
    assert.equal(
      splitRight.sourceStart,
      splitLeft.sourceStart,
      "both retained sides must share the original clip's source phase",
    );
    assert.equal(
      replacementMidiClip.sourceStart,
      replacementMidiClip.start,
      "new MIDI material must begin a new source phase",
    );
    assert.ok(
      splitRight.sourceStart < splitRight.start,
      "the retained right side must keep a phase anchor before its visible start",
    );
    assert.ok(
      splitLeft.id !== splitRight.id && splitLeft.events[0].id === splitRight.events[0].id,
      "region replacement must expose the duplicate event-ID focus scenario",
    );
    const splitEventId = splitRight.events[0].id;
    const splitBeatDuration = 60 / splitClipProject.bpm;
    const splitLoopDuration = splitRight.loopBeats * splitBeatDuration;
    const expectedPhaseEvent = splitRight.events
      .map((event) => {
        const eventOffset = event.time * splitBeatDuration;
        const cycle = Math.max(
          0,
          Math.ceil((splitRight.start - splitRight.sourceStart - eventOffset - 0.000001) / splitLoopDuration),
        );
        return {
          event,
          onset: splitRight.sourceStart + cycle * splitLoopDuration + eventOffset,
        };
      })
      .filter((occurrence) => occurrence.onset >= splitRight.start - 0.000001 && occurrence.onset < splitRight.end)
      .sort((left, right) => left.onset - right.onset)[0];
    const rightEventIds = splitRight.events.map((event) => event.id);
    const phaseStartSteps = Math.round(splitRight.start / 0.25);
    await evaluate(cdp, appSession, `(() => {
      const lane = document.querySelector('.track-lane');
      lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'Home', bubbles: true, cancelable: true }));
      for (let index = 0; index < ${phaseStartSteps}; index += 1) {
        lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true, cancelable: true }));
      }
      window.__oscillators = [];
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `window.__oscillators.some(
            (node) => node.dawAiTrackId === ${splitLead.id} &&
              ${JSON.stringify(rightEventIds)}.includes(node.dawAiEventId) && !node.dawAiChased,
          )`,
        ),
      "retained right-side MIDI phase playback",
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        `window.__oscillators.find(
          (node) => node.dawAiTrackId === ${splitLead.id} &&
            ${JSON.stringify(rightEventIds)}.includes(node.dawAiEventId) && !node.dawAiChased,
        ).dawAiEventId`,
      ),
      expectedPhaseEvent.event.id,
      `the retained right side must continue with its source-phase onset at ${expectedPhaseEvent.onset}`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer for split clip focus",
    );
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector(
        '[data-sound-tool="event"][data-track-id="${splitLead.id}"][data-clip-id="${splitRight.id}"][data-tool-id="${splitEventId}"][data-parameter="pitch"]',
      );
      input.value = '80';
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const current = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      const lead = current.tracks.find((track) => track.id === splitLead.id);
      return lead.clips.find((clip) => clip.id === splitRight.id).events[0].pitch === 80;
    }, "right split clip event update");
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        clipId: document.activeElement.dataset.clipId,
        key: document.activeElement.dataset.controlKey,
      })`),
      {
        clipId: String(splitRight.id),
        key: `${splitLead.id}-clip-${splitRight.id}-event-${splitEventId}-pitch`,
      },
      "event focus must return to the edited owning clip",
    );
    await evaluate(cdp, appSession, `(() => {
      const update = (parameter, value) => {
        const control = document.querySelector(
          '[data-sound-tool="modulator"][data-track-id="${splitLead.id}"][data-parameter="' + parameter + '"]',
        );
        control.value = value;
        control.dispatchEvent(new Event('change', { bubbles: true }));
      };
      update('shape', 'square');
      update('rate', '0.5');
      update('depth', '1');
      update('target', 'instrument.attack');
    })()`);
    await waitFor(async () => {
      const current = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      const lead = current.tracks.find((track) => track.id === splitLead.id);
      const modulator = lead.modulators[0];
      return modulator.shape === "square" && modulator.parameters.rate === 0.5 &&
        modulator.parameters.depth === 1 && modulator.target === "instrument.attack";
    }, "split lead attack modulation configuration");
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * (3.25 / 32), lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * (3.25 / 32), lane.y);
    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      window.__oscillators = [];
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `window.__oscillators.some(
            (node) => node.dawAiTrackId === ${splitLead.id} &&
              node.dawAiEventId === ${precedingClipEvent.id} && node.dawAiChased,
          )`,
        ),
      "preceding clip voice chased during its onset attack",
    );
    const crossClipAttack = await evaluate(cdp, appSession, `(() => {
      const oscillator = window.__oscillators.find(
        (node) => node.dawAiTrackId === ${splitLead.id} && node.dawAiEventId === ${precedingClipEvent.id},
      );
      const envelope = window.__gainNodes.find(
        (node) => node.dawAiVoiceEnvelope && node.dawAiTrackId === ${splitLead.id} &&
          node.dawAiEventId === ${precedingClipEvent.id},
      );
      return {
        attack: oscillator.dawAiInstrumentAttack,
        currentLevel: envelope.gain.dawAiSetValues[0].value,
        tailLevel: envelope.gain.dawAiExponentialRamps.at(-1).value,
      };
    })()`);
    assert.ok(
      crossClipAttack.attack > 0.5 && crossClipAttack.currentLevel > crossClipAttack.tailLevel,
      `a cross-clip chase must retain onset-sampled attack state (${JSON.stringify(crossClipAttack)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * (3.3 / 32), lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * (3.3 / 32), lane.y);
    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      window.__oscillators = [];
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `window.__oscillators.some(
            (node) => node.dawAiTrackId === ${splitLead.id} &&
              node.dawAiEventId === ${precedingClipEvent.id} && node.dawAiChased,
          )`,
        ),
      "preceding clip release tail chase",
    );
    assert.ok(
      await evaluate(cdp, appSession, `(() => {
        const oscillator = window.__oscillators.find(
          (node) => node.dawAiTrackId === ${splitLead.id} && node.dawAiEventId === ${precedingClipEvent.id},
        );
        return oscillator.dawAiStopTime > oscillator.dawAiStartTime;
      })()`),
      "a release tail from the preceding clip must remain sounding after resume",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await evaluate(cdp, appSession, `window.__audioCloseCount = ${audioCloseCountBeforeGenreEdit}`);
    for (const [condition, label] of [
      ["modulator.target === 'instrument.pitch'", "split lead attack target undo"],
      ["modulator.parameters.depth === 0.08", "split lead modulation depth undo"],
      ["modulator.parameters.rate === 5", "split lead modulation rate undo"],
      ["modulator.shape === 'sine'", "split lead modulation shape undo"],
    ]) {
      await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
      await waitFor(
        async () =>
          evaluate(
            cdp,
            appSession,
            `fetch('/api/project').then((response) => response.json()).then((project) => {
              const lead = project.tracks.find((track) => track.id === ${splitLead.id});
              const modulator = lead.modulators[0];
              return ${condition};
            })`,
          ),
        label,
      );
    }
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const current = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      const lead = current.tracks.find((track) => track.id === splitLead.id);
      return lead.clips.find((clip) => clip.id === splitRight.id).events[0].pitch === 69;
    }, "split clip event undo");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 1"),
      "split MIDI replacement undo",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "document.querySelectorAll('.edit-item').length === 0 && document.querySelectorAll('.track-row').length === 3",
        ),
      "split lead undo",
    );

    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * (0.25 / 32), lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * (0.5 / 32), lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * (0.5 / 32), lane.y);
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('#prompt-input');
      input.value = 'add texture';
      document.querySelector('#prompt-form').requestSubmit();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.track-row').length === 4"),
      "quarter-second generated clip",
    );
    const shortClipProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    const textureTrack = shortClipProject.tracks.find((track) => track.role === "texture");
    assert.deepEqual(
      textureTrack.clips.map((clip) => [clip.start, clip.end]),
      [[0.25, 0.5]],
      "the generated phrase must preserve the short selection",
    );
    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      window.__oscillators = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `window.__oscillators.some((node) => node.dawAiTrackId === ${textureTrack.id})`,
        ),
      "clip-relative short phrase audio",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#play-button').classList.contains('is-playing')"),
      "short phrase pause",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.track-row').length === 3"),
      "short phrase undo",
    );

    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('#prompt-input');
      input.value = 'mute chords';
      document.querySelector('#prompt-form').requestSubmit();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 1"),
      "short regional mute",
    );
    const mutedProject = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
    const chordTrack = mutedProject.tracks.find((track) => track.role === "chords");
    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      window.__oscillators = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    const chordGate = `window.__gainNodes.find((node) => node.dawAiAutomation === 'level' && node.dawAiTrackId === ${chordTrack.id})`;
    const chordAudibleBeforeMute = await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `Boolean(${chordGate}) && ${chordGate}.gain.dawAiSetValues?.some((entry) => entry.value === 0)`,
        ),
      "scheduled chord channel mute",
    );
    assert.ok(chordAudibleBeforeMute, "the regional mute must be scheduled on the chord channel");
    const chordGateValues = await evaluate(
      cdp,
      appSession,
      `${chordGate}.gain.dawAiSetValues.map((entry) => entry.value)`,
    );
    const firstMutedChordValue = chordGateValues.indexOf(0);
    assert.ok(
      chordGateValues.slice(0, firstMutedChordValue).some((value) => value > 0.5) &&
        chordGateValues.slice(firstMutedChordValue + 1).some((value) => value > 0.5),
      "the chord channel must be audible before and after the regional mute",
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        `window.__oscillators.some((node) => node.dawAiTrackId === ${chordTrack.id})`,
      ),
      true,
      "the boundary gate must affect a chord voice that started before the edit",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 0"),
      "regional mute undo",
    );

    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * 0.25, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * 0.5, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.5, lane.y);
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('#prompt-input');
      input.value = 'add echo to the chords';
      document.querySelector('#prompt-form').requestSubmit();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 1"),
      "echo edit",
    );
    const echoProject = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
    assert.equal(echoProject.tracks.length, 3, "echo prompt must not add a track");
    assert.equal(echoProject.edits[0].action.name, "Echo");
    assert.equal(
      echoProject.tracks.find((track) => track.role === "chords").effects.some((effect) => effect.name === "Echo"),
      false,
      "regional effects must not mutate the baseline channel chain",
    );
    assert.match(
      await evaluate(cdp, appSession, "document.querySelector('.effect-pill.is-regional').textContent"),
      /Echo.*8\.0-16\.0s/,
      "advanced view must expose the regional effect and its range",
    );

    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      window.__delayNodes = [];
      window.__convolverCount = 0;
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#play-button').classList.contains('is-playing')"),
      "transport playback",
    );
    await waitFor(
      async () =>
        evaluate(cdp, appSession, "window.__delayNodes.filter((node) => node.dawAiEffect === 'echo').length === 3"),
      "audio graph creation",
    );
    assert.equal(await evaluate(cdp, appSession, "window.__convolverCount"), 3, "each track must have a reverb bus");
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        "window.__delayNodes.filter((node) => node.dawAiEffect === 'echo').every((node) => Math.abs(node.delayTime.value - 60 / 112 / 2) < 0.01)",
      ),
      true,
      "echo buses must use an eighth-note delay",
    );
    const echoSend = `window.__gainNodes.find((node) => node.dawAiAutomation === 'echo' && node.dawAiTrackId === ${chordTrack.id})`;
    assert.equal(
      await evaluate(cdp, appSession, `${echoSend}.gain.value`),
      0,
      "regional echo must be inactive before its selected range",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#play-button').classList.contains('is-playing')"),
      "first transport pause",
    );

    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * 0.25, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.25, lane.y);
    await evaluate(cdp, appSession, "window.__gainNodes = []");
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    const activeEchoSend = `window.__gainNodes.find((node) => node.dawAiAutomation === 'echo' && node.dawAiTrackId === ${chordTrack.id})`;
    await waitFor(
      async () => evaluate(cdp, appSession, `Boolean(${activeEchoSend}) && ${activeEchoSend}.gain.value > 0`),
      "regional echo send inside the selected range",
    );
    assert.ok(
      Math.abs((await evaluate(cdp, appSession, `${activeEchoSend}.gain.value`)) - 0.34 * 0.55) < 0.001,
      "the selected region must control the chord echo send",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#play-button').classList.contains('is-playing')"),
      "transport pause",
    );
    assert.equal(await evaluate(cdp, appSession, "window.__audioCloseCount"), 5, "each pause must close the audio graph");

    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 0"),
      "echo undo before rhythm checks",
    );
    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * 0.25, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * 0.5, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.5, lane.y);
    await submitPrompt(cdp, appSession, "remove reverb from the chords", 1);
    const removalProject = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
    assert.deepEqual(removalProject.edits[0].action, {
      type: "remove-effect",
      name: "Reverb",
      target: "chords",
    });
    assert.match(
      await evaluate(cdp, appSession, "document.querySelector('.effect-pill.is-regional').textContent"),
      /Reverb.*OFF.*8\.0-16\.0s/,
      "advanced view must identify regional effect removal",
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        "[...document.querySelectorAll('.effect-pill:not(.is-regional)')].some((pill) => /Room.*20%/.test(pill.textContent))",
      ),
      true,
      "Advanced must continue to display the independent Room effect",
    );
    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    const baselineReverb = `window.__gainNodes.find((node) => node.dawAiAutomation === 'reverb' && node.dawAiTrackId === ${chordTrack.id})`;
    await waitFor(
      async () => evaluate(cdp, appSession, `Boolean(${baselineReverb}) && ${baselineReverb}.gain.value > 0`),
      "baseline chord reverb send",
    );
    const baselineRoomSend = await evaluate(cdp, appSession, `${baselineReverb}.gain.value`);
    assert.ok(
      baselineRoomSend > 0,
      "baseline chord reverb must remain active before the removal region",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * 0.25, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.25, lane.y);
    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      document.querySelector('#play-button').click();
    })()`);
    const removedReverb = `window.__gainNodes.find((node) => node.dawAiAutomation === 'reverb' && node.dawAiTrackId === ${chordTrack.id})`;
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#play-button').classList.contains('is-playing') &&
            Boolean(${removedReverb}) && ${removedReverb}.gain.value > 0`,
        ),
      "remaining chord Room send",
    );
    const roomSendAfterRemoval = await evaluate(cdp, appSession, `${removedReverb}.gain.value`);
    assert.ok(
      Math.abs(roomSendAfterRemoval - baselineRoomSend) < 0.001,
      `removing Reverb must preserve the independent Room send (before ${baselineRoomSend}, after ${roomSendAfterRemoval})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 0"),
      "effect removal undo before rhythm checks",
    );
    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * 0.25, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * 0.5, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.5, lane.y);
    await submitPrompt(cdp, appSession, "remove effects from the bass", 1);
    const allEffectsProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.deepEqual(allEffectsProject.edits[0].action, {
      type: "remove-effect",
      name: "Effects",
      target: "bass",
    });
    const bassTrackId = allEffectsProject.tracks.find((track) => track.role === "bass").id;
    await evaluate(cdp, appSession, `(() => {
      window.__gainNodes = [];
      window.__biquadFilters = [];
      document.querySelector('#play-button').click();
    })()`);
    const bassFilterBypass = `window.__gainNodes.find((node) => node.dawAiAutomation === 'filter-bypass' && node.dawAiTrackId === ${bassTrackId})`;
    const bassToneFilter = `window.__biquadFilters.find((node) => node.dawAiAutomation === 'tone' && node.dawAiTrackId === ${bassTrackId})`;
    const bassEffectFilter = `window.__biquadFilters.find((node) => node.dawAiAutomation === 'effect-filter' && node.dawAiTrackId === ${bassTrackId})`;
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `Boolean(${bassFilterBypass}) && ${bassFilterBypass}.gain.value === 1 &&
            Boolean(${bassToneFilter}) && Boolean(${bassEffectFilter}) &&
            ${bassToneFilter}.frequency.dawAiSetValues?.length > 20`,
        ),
      "generic effect removal to bypass the bass channel filter",
    );
    const toneDuringEffectRemoval = await evaluate(cdp, appSession, `(() => {
      const tone = ${bassToneFilter};
      const effect = ${bassEffectFilter};
      const bypass = ${bassFilterBypass};
      const values = tone.frequency.dawAiSetValues.map((entry) => entry.value);
      return {
        independentStage: bypass.dawAiBypassedStage === effect && bypass.dawAiBypassedStage !== tone,
        spread: Math.max(...values) - Math.min(...values),
      };
    })()`);
    assert.ok(
      toneDuringEffectRemoval.independentStage && toneDuringEffectRemoval.spread > 1,
      `instrument tone must remain independently modulated while effects are bypassed (${JSON.stringify(toneDuringEffectRemoval)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 0"),
      "generic effect removal undo before rhythm checks",
    );
    await mouse(cdp, appSession, "mousePressed", lane.left + 1, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.right - 1, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.right - 1, lane.y);
    await submitPrompt(cdp, appSession, "add lead", 1);
    await submitPrompt(cdp, appSession, "add texture", 2);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.track-row').length === 5"),
      "pitched rhythm test tracks",
    );
    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer for authoritative rhythm pitch",
    );
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="pitch"]');
      input.value = '80';
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].clips[0].events[0].pitch === 80;
    }, "authoritative bass event pitch update");
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await submitPrompt(cdp, appSession, "make the chords busy", 3);
    await submitPrompt(cdp, appSession, "make the bass busy", 4);
    await submitPrompt(cdp, appSession, "make the lead sparse", 5);
    await submitPrompt(cdp, appSession, "make the texture busy", 6);
    const rhythmProject = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
    const roleIds = Object.fromEntries(rhythmProject.tracks.map((track) => [track.role, track.id]));
    await evaluate(cdp, appSession, `(() => {
      window.__oscillators = [];
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `(() => {
            const count = (id) => window.__oscillators.filter((node) => node.dawAiTrackId === id).length;
            return count(${roleIds.chords}) >= 13 && count(${roleIds.bass}) >= 5 &&
              count(${roleIds.lead}) >= 3 && count(${roleIds.texture}) >= 6;
          })()`,
        ),
      "targeted pitched rhythm events",
    );
    const rhythmGaps = await evaluate(cdp, appSession, `(() => {
      const gap = (id) => {
        const starts = [...new Set(window.__oscillators
          .filter((node) => node.dawAiTrackId === id)
          .map((node) => node.dawAiStartTime.toFixed(4)))]
          .map(Number)
          .sort((left, right) => left - right);
        return starts.at(-1) - starts.at(-2);
      };
      return {
        chords: gap(${roleIds.chords}),
        bass: gap(${roleIds.bass}),
        lead: gap(${roleIds.lead}),
        texture: gap(${roleIds.texture}),
      };
    })()`);
    const sixteenth = 60 / rhythmProject.bpm / 4;
    assert.ok(
      Math.abs(rhythmGaps.chords - sixteenth * 4) < 0.02,
      `busy chords must double their cadence (observed ${rhythmGaps.chords})`,
    );
    assert.ok(
      Math.abs(rhythmGaps.bass - sixteenth * 2) < 0.02,
      `busy bass must double its graph-event cadence (observed ${rhythmGaps.bass})`,
    );
    assert.ok(
      Math.abs(rhythmGaps.lead - sixteenth * 8) < 0.02,
      `sparse lead must halve its cadence (observed ${rhythmGaps.lead})`,
    );
    assert.ok(
      Math.abs(rhythmGaps.texture - sixteenth * 8) < 0.02,
      `busy texture must double its cadence (observed ${rhythmGaps.texture})`,
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        `window.__oscillators.some((node) => node.dawAiTrackId === ${roleIds.bass} && Math.abs(node.dawAiBaseFrequency - 830.6094) < 0.1)`,
      ),
      true,
      "rhythm density must preserve the edited MIDI 80 bass event",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");

    attacker = await startAttackerServer(attackerPort);
    const attackerSession = await openPage(cdp, `http://127.0.0.1:${attackerPort}`);
    await evaluate(cdp, attackerSession, `fetch('${appUrl}/api/edits', {
      method: 'POST',
      mode: 'no-cors',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: 'start=1&end=2&prompt=hostile+edit'
    }).then(() => true).catch(() => false)`);
    await evaluate(cdp, attackerSession, `fetch('${appUrl}/api/sound-tools', {
      method: 'POST',
      mode: 'no-cors',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: 'track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth'
    }).then(() => true).catch(() => false)`);
    const afterAttack = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
    assert.equal(afterAttack.edits.some((edit) => edit.prompt === "hostile edit"), false);
    assert.equal(
      afterAttack.tracks.find((track) => track.id === 2).instrument.waveform,
      "square",
      "cross-origin sound-tool mutations must be rejected",
    );
    assert.equal(consoleErrors.length, 0, "application emitted browser console errors");

    console.log(
      "Browser workflows passed: mobile layout/panning, keyboard selection, serialized transport, exact event/envelope playback, voice chase, AI-authored MIDI composition, regional effects/filtering, short clips, targeted rhythm, complete modulation routing, advanced sound tools, prompt single-flight/undo, mixer focus/transport, modal, cross-origin guard",
    );
  } finally {
    if (attacker) await new Promise((resolve) => attacker.close(resolve));
    await closeBrowser(cdp, chrome);
    await terminate(app);
    await removeBrowserProfile(profile);
  }

  if (appErrors) process.stderr.write(appErrors);
  if (chrome.exitCode && chromeErrors) process.stderr.write(chromeErrors);
}

run().catch((error) => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
