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
    const browserWebSocket = await waitFor(async () => {
      const response = await fetch(`http://127.0.0.1:${debugPort}/json/version`);
      if (!response.ok) return false;
      return (await response.json()).webSocketDebuggerUrl;
    }, "Chrome DevTools endpoint");
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

    const promptSingleFlight = await evaluate(cdp, appSession, `(() => {
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
      return {
        requests: window.__promptRequestCount,
        submitDisabled: document.querySelector('#compose-button').disabled,
      };
    })()`);
    assert.deepEqual(
      promptSingleFlight,
      { requests: 1, submitDisabled: true },
      "prompt shortcuts must share one in-flight edit request",
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
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 0"),
      "single-flight prompt undo",
    );

    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('#prompt-input');
      input.value = 'make the chords warm and spacious';
      document.querySelector('#prompt-form').requestSubmit();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 1"),
      "compound AI edit",
    );
    const compoundProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.equal(compoundProject.tracks.length, 3, "effect prompt must not add a track");
    assert.equal(compoundProject.edits[0].action.type, "compound");
    assert.deepEqual(
      compoundProject.edits[0].action.actions.map((action) => action.type),
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

    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.edit-item').length === 0"),
      "undo",
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
    const originalVersion = compoundProject.version + 1;
    await evaluate(cdp, appSession, "document.querySelector('[data-volume-track]').focus()");
    await pressKey(cdp, appSession, "ArrowRight", "ArrowRight", 39);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.version > originalVersion && Math.abs(project.tracks[0].volume - 0.83) < 0.001;
    }, "mixer change");
    assert.equal(
      await evaluate(cdp, appSession, "document.activeElement.dataset.volumeTrack"),
      "1",
      "mixer updates must restore focus to the adjusted control",
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
      window.__delayNodes = [];
      window.__convolverCount = 0;
      window.__reverbBuffers = [];
      window.__noiseBuffers = [];
      const originalClose = AudioContext.prototype.close;
      const originalCreateBuffer = AudioContext.prototype.createBuffer;
      const originalGain = AudioContext.prototype.createGain;
      const originalOscillator = AudioContext.prototype.createOscillator;
      const originalOscillatorStart = OscillatorNode.prototype.start;
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
      OscillatorNode.prototype.start = function start(when = 0) {
        this.dawAiStartTime = when;
        return originalOscillatorStart.call(this, when);
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
    await waitFor(
      async () => evaluate(cdp, appSession, `Boolean(${chordGate}) && ${chordGate}.gain.value > 0.5`),
      "audible chord channel gate",
    );
    assert.ok(
      await evaluate(cdp, appSession, `${chordGate}.gain.value > 0.5`),
      "the chord voice must be audible before the regional mute",
    );
    await waitFor(
      async () => evaluate(cdp, appSession, `${chordGate}.gain.value === 0`),
      "active chord voice to mute at the edit boundary",
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
    await waitFor(
      async () => evaluate(cdp, appSession, `${chordGate}.gain.value > 0.5`),
      "chord channel to reopen after the edit boundary",
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
      document.querySelector('#play-button').click();
    })()`);
    const bassFilterBypass = `window.__gainNodes.find((node) => node.dawAiAutomation === 'filter-bypass' && node.dawAiTrackId === ${bassTrackId})`;
    await waitFor(
      async () => evaluate(cdp, appSession, `Boolean(${bassFilterBypass}) && ${bassFilterBypass}.gain.value === 1`),
      "generic effect removal to bypass the bass channel filter",
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
    await submitPrompt(cdp, appSession, "make the chords busy", 3);
    await submitPrompt(cdp, appSession, "make the bass sparse", 4);
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
            return count(${roleIds.chords}) >= 9 && count(${roleIds.bass}) >= 3 &&
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
        return starts[2] - starts[1];
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
      Math.abs(rhythmGaps.bass - sixteenth * 8) < 0.02,
      `sparse bass must halve its cadence (observed ${rhythmGaps.bass})`,
    );
    assert.ok(
      Math.abs(rhythmGaps.lead - sixteenth * 8) < 0.02,
      `sparse lead must halve its cadence (observed ${rhythmGaps.lead})`,
    );
    assert.ok(
      Math.abs(rhythmGaps.texture - sixteenth * 8) < 0.02,
      `busy texture must double its cadence (observed ${rhythmGaps.texture})`,
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
    const afterAttack = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
    assert.equal(afterAttack.edits.some((edit) => edit.prompt === "hostile edit"), false);
    assert.equal(consoleErrors.length, 0, "application emitted browser console errors");

    console.log(
      "Browser workflows passed: mobile page/timeline panning, keyboard selection, serialized transport, voice chase, regional effects/filtering, short clips, targeted rhythm, prompt single-flight/undo, mixer focus/transport, modal, cross-origin guard",
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
