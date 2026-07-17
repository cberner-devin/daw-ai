(() => {
  "use strict";

  const elements = {
    projectName: document.querySelector("#project-name"),
    tempo: document.querySelector("#tempo"),
    currentTime: document.querySelector("#current-time"),
    totalTime: document.querySelector("#total-time"),
    playButton: document.querySelector("#play-button"),
    rewindButton: document.querySelector("#rewind-button"),
    timelineContent: document.querySelector("#timeline-content"),
    timelineScroll: document.querySelector("#timeline-scroll"),
    rulerLane: document.querySelector("#ruler-lane"),
    trackRows: document.querySelector("#track-rows"),
    selection: document.querySelector("#timeline-selection"),
    playhead: document.querySelector("#playhead"),
    selectionReadout: document.querySelector("#selection-readout"),
    selectionModeButton: document.querySelector("#selection-mode-button"),
    promptRange: document.querySelector("#prompt-range"),
    promptForm: document.querySelector("#prompt-form"),
    promptInput: document.querySelector("#prompt-input"),
    composeButton: document.querySelector("#compose-button"),
    undoButton: document.querySelector("#undo-button"),
    resetButton: document.querySelector("#reset-button"),
    savedState: document.querySelector("#saved-state"),
    editLog: document.querySelector("#edit-log"),
    editCount: document.querySelector("#edit-count"),
    advancedButton: document.querySelector("#advanced-button"),
    closeAdvanced: document.querySelector("#close-advanced"),
    advancedDrawer: document.querySelector("#advanced-drawer"),
    drawerBackdrop: document.querySelector("#drawer-backdrop"),
    channelList: document.querySelector("#channel-list"),
    toast: document.querySelector("#toast"),
  };

  const state = {
    project: null,
    selectionStart: 8,
    selectionEnd: 16,
    dragPointer: null,
    dragAnchor: 0,
    touchSelectionMode: false,
    promptPending: false,
    centeredInitialSelection: false,
    toastTimer: null,
    drawerHideTimer: null,
  };

  class AudioEngine {
    constructor() {
      this.context = null;
      this.master = null;
      this.playbackState = "idle";
      this.playbackGeneration = 0;
      this.playhead = 0;
      this.contextStartedAt = 0;
      this.projectStartedAt = 0;
      this.nextStep = 0;
      this.timer = null;
      this.frame = null;
      this.noiseBuffer = null;
      this.reverbImpulse = null;
      this.trackGraphs = new Map();
      this.activeSources = new Set();
    }

    get isPlaying() {
      return this.playbackState === "playing";
    }

    get isActive() {
      return this.playbackState !== "idle";
    }

    async toggle() {
      if (this.isActive) {
        this.stop(true);
      } else {
        await this.start();
      }
    }

    async start() {
      if (!state.project || this.isActive) return;
      this.playbackState = "starting";
      this.playbackGeneration += 1;
      const generation = this.playbackGeneration;
      if (!this.context) this.createContext();
      const context = this.context;
      updateTransport();
      try {
        await context.resume();
      } catch (error) {
        if (generation !== this.playbackGeneration) return;
        this.stop(true);
        showToast("Could not start audio: " + error.message, true);
        return;
      }
      if (
        generation !== this.playbackGeneration ||
        this.playbackState !== "starting" ||
        this.context !== context
      ) {
        return;
      }
      if (this.playhead >= state.project.duration - 0.01) this.playhead = 0;

      this.createTrackGraphs();
      this.playbackState = "playing";
      this.contextStartedAt = this.context.currentTime;
      this.projectStartedAt = this.playhead;
      this.scheduleTrackAutomation();
      const stepDuration = this.stepDuration();
      this.chaseActiveVoices(stepDuration);
      this.nextStep = Math.ceil(this.playhead / stepDuration) * stepDuration;
      this.pump();
      this.timer = window.setInterval(() => this.pump(), 70);
      this.animate();
      updateTransport();
    }

    stop(preservePosition) {
      if (this.isPlaying && preservePosition) this.updatePosition();
      this.playbackGeneration += 1;
      this.playbackState = "idle";
      window.clearInterval(this.timer);
      window.cancelAnimationFrame(this.frame);
      this.timer = null;
      this.frame = null;
      if (this.context) {
        const context = this.context;
        const now = context.currentTime;
        this.master.gain.cancelScheduledValues(now);
        this.master.gain.setValueAtTime(0.0001, now);
        for (const source of this.activeSources) {
          try {
            source.stop(now);
          } catch (error) {
            if (error.name !== "InvalidStateError") throw error;
          }
        }
        this.activeSources.clear();
        this.context = null;
        this.master = null;
        this.reverbImpulse = null;
        this.trackGraphs.clear();
        this.noiseBuffer = null;
        void context.close().catch(() => {});
      }
      if (!preservePosition) this.playhead = 0;
      updateTransport();
      renderPlayhead();
    }

    seek(time) {
      const wasActive = this.isActive;
      if (wasActive) this.stop(true);
      this.playhead = clamp(time, 0, state.project?.duration ?? 0);
      renderPlayhead();
      updateTransport();
      if (wasActive) void this.start();
    }

    createContext() {
      const AudioContext = window.AudioContext || window.webkitAudioContext;
      this.context = new AudioContext();
      const compressor = this.context.createDynamicsCompressor();
      compressor.threshold.value = -12;
      compressor.knee.value = 16;
      compressor.ratio.value = 5;
      this.master = this.context.createGain();
      this.master.gain.value = 0.58;
      this.master.connect(compressor);
      compressor.connect(this.context.destination);

      this.reverbImpulse = this.createReverbImpulse();

      const noiseRandom = this.seededRandom(0x4e4f4953);
      this.noiseBuffer = this.context.createBuffer(
        1,
        Math.floor(this.context.sampleRate * 0.5),
        this.context.sampleRate,
      );
      const data = this.noiseBuffer.getChannelData(0);
      for (let index = 0; index < data.length; index += 1) {
        data[index] = noiseRandom() * 2 - 1;
      }
    }

    createTrackGraphs() {
      this.trackGraphs.clear();
      for (const track of state.project.tracks) {
        const input = this.context.createGain();
        const filter = this.context.createBiquadFilter();
        const filterOutput = this.context.createGain();
        const filterBypass = this.context.createGain();
        const gate = this.context.createGain();
        const echoSend = this.context.createGain();
        const delay = this.context.createDelay(2);
        const delayFeedback = this.context.createGain();
        const reverbSend = this.context.createGain();
        const reverb = this.context.createConvolver();
        const chorusSend = this.context.createGain();
        const chorusDelay = this.context.createDelay(0.05);
        const compressorSend = this.context.createGain();
        const compressor = this.context.createDynamicsCompressor();

        filter.type = "lowpass";
        filterOutput.gain.value = 0;
        filterBypass.gain.value = 0;
        gate.gain.value = 0;
        echoSend.gain.value = 0;
        reverbSend.gain.value = 0;
        chorusSend.gain.value = 0;
        compressorSend.gain.value = 0;
        delay.delayTime.value = 60 / state.project.bpm / 2;
        delay.dawAiEffect = "echo";
        delayFeedback.gain.value = 0.24;
        reverb.buffer = this.reverbImpulse;
        chorusDelay.delayTime.value = 0.018;
        chorusDelay.dawAiEffect = "chorus";
        compressor.threshold.value = -22;
        compressor.knee.value = 18;
        compressor.ratio.value = 7;
        compressor.attack.value = 0.004;
        compressor.release.value = 0.12;
        gate.dawAiAutomation = "level";
        gate.dawAiTrackId = track.id;
        filterOutput.dawAiAutomation = "filter";
        filterOutput.dawAiTrackId = track.id;
        filterBypass.dawAiAutomation = "filter-bypass";
        filterBypass.dawAiTrackId = track.id;
        echoSend.dawAiAutomation = "echo";
        echoSend.dawAiTrackId = track.id;
        reverbSend.dawAiAutomation = "reverb";
        reverbSend.dawAiTrackId = track.id;
        chorusSend.dawAiAutomation = "chorus";
        chorusSend.dawAiTrackId = track.id;
        compressorSend.dawAiAutomation = "compressor";
        compressorSend.dawAiTrackId = track.id;

        input.connect(filter);
        input.connect(filterBypass);
        filter.connect(filterOutput);
        filterOutput.connect(gate);
        filterBypass.connect(gate);
        filter.connect(echoSend);
        filter.connect(reverbSend);
        filter.connect(chorusSend);
        filter.connect(compressorSend);
        echoSend.connect(delay);
        delay.connect(gate);
        delay.connect(delayFeedback);
        delayFeedback.connect(delay);
        reverbSend.connect(reverb);
        reverb.connect(gate);
        chorusSend.connect(chorusDelay);
        chorusDelay.connect(gate);
        compressorSend.connect(compressor);
        compressor.connect(gate);
        gate.connect(this.master);
        this.trackGraphs.set(track.id, {
          input,
          filter,
          filterOutput,
          filterBypass,
          gate,
          echoSend,
          reverbSend,
          chorusSend,
          compressorSend,
        });
      }
    }

    scheduleTrackAutomation() {
      for (const track of state.project.tracks) {
        const boundaries = new Set([this.projectStartedAt]);
        for (const edit of state.project.edits) {
          if (edit.start >= this.projectStartedAt) boundaries.add(edit.start);
          if (edit.end >= this.projectStartedAt) boundaries.add(edit.end);
        }
        for (const clip of track.clips) {
          if (clip.start >= this.projectStartedAt) boundaries.add(clip.start);
          if (clip.end >= this.projectStartedAt) boundaries.add(clip.end);
        }
        const orderedBoundaries = [...boundaries].sort((left, right) => left - right);
        const graph = this.trackGraphs.get(track.id);
        for (const boundary of orderedBoundaries) {
          const audioTime = Math.max(
            this.context.currentTime,
            this.contextStartedAt + boundary - this.projectStartedAt,
          );
          const automation = this.automationAt(track, boundary);
          graph.gate.gain.setValueAtTime(automation.gain, audioTime);
          graph.filterOutput.gain.setValueAtTime(automation.filterBypass ? 0 : 1, audioTime);
          graph.filterBypass.gain.setValueAtTime(automation.filterBypass ? 1 : 0, audioTime);
          graph.echoSend.gain.setValueAtTime(Math.min(0.6, automation.echo * 0.55), audioTime);
          const reverbMix = Math.max(
            automation.reverb.reverb,
            automation.reverb.room,
            automation.reverb.shimmer,
          );
          graph.reverbSend.gain.setValueAtTime(Math.min(0.6, reverbMix * 0.7), audioTime);
          graph.chorusSend.gain.setValueAtTime(Math.min(0.5, automation.chorus * 0.5), audioTime);
          graph.compressorSend.gain.setValueAtTime(Math.min(0.5, automation.compression * 0.45), audioTime);
          graph.filter.frequency.setValueAtTime(
            clamp(
              this.baseFilterForRole(track.role) * (1 + automation.filter) * (1 - automation.lowPass * 0.65),
              180,
              9000,
            ),
            audioTime,
          );
        }
      }
    }

    automationAt(track, time) {
      const clipActive = track.clips.some((clip) => time >= clip.start && time < clip.end);
      const automation = {
        gain: track.muted || !clipActive ? 0 : track.volume,
        filter: 0,
        filterBypass: false,
        lowPass: 0,
        echo: 0,
        reverb: {
          reverb: 0,
          room: 0,
          shimmer: 0,
        },
        chorus: 0,
        compression: 0,
      };
      for (const effect of track.effects) {
        if (effect.enabled) this.applyEffect(effect.name, effect.mix, automation);
      }
      for (const edit of state.project.edits) {
        if (time >= edit.start && time < edit.end) {
          this.applyAutomationAction(edit.action, track.role, automation);
        }
      }
      return automation;
    }

    applyAutomationAction(action, role, automation) {
      if (action.type === "compound") {
        for (const child of action.actions) this.applyAutomationAction(child, role, automation);
        return;
      }
      if (action.target !== "all" && action.target !== role) return;
      if (action.type === "gain") automation.gain *= action.value;
      if (action.type === "mute") automation.gain = 0;
      if (action.type === "filter") {
        automation.filter += action.value;
        automation.filterBypass = false;
      }
      if (action.type === "effect") this.applyEffect(action.name, action.value, automation);
      if (action.type === "remove-effect") this.removeEffect(action.name, automation);
      if (action.type === "drop") automation.gain *= 1.08;
    }

    applyEffect(name, mix, automation) {
      const normalized = name.toLowerCase();
      if (normalized.includes("echo") || normalized.includes("delay")) {
        automation.echo = Math.max(automation.echo, mix);
      }
      if (normalized === "reverb") automation.reverb.reverb = Math.max(automation.reverb.reverb, mix);
      if (normalized === "room") automation.reverb.room = Math.max(automation.reverb.room, mix);
      if (normalized === "shimmer") automation.reverb.shimmer = Math.max(automation.reverb.shimmer, mix);
      if (normalized.includes("chorus")) automation.chorus = Math.max(automation.chorus, mix);
      if (normalized.includes("compressor") || normalized.includes("compression")) {
        automation.compression = Math.max(automation.compression, mix);
      }
      if (normalized.includes("low-pass") || normalized.includes("low pass") || normalized.includes("filter")) {
        automation.lowPass = Math.max(automation.lowPass, mix);
        automation.filterBypass = false;
      }
    }

    removeEffect(name, automation) {
      const normalized = name.toLowerCase();
      const removeAll = normalized === "effect" || normalized === "effects" || normalized === "fx";
      if (normalized.includes("echo") || normalized.includes("delay") || removeAll) {
        automation.echo = 0;
      }
      if (normalized === "reverb" || removeAll) automation.reverb.reverb = 0;
      if (normalized === "room" || removeAll) automation.reverb.room = 0;
      if (normalized === "shimmer" || removeAll) automation.reverb.shimmer = 0;
      if (removeAll) {
        automation.filter = 0;
      }
      if (normalized.includes("chorus") || removeAll) automation.chorus = 0;
      if (normalized.includes("compressor") || normalized.includes("compression") || removeAll) {
        automation.compression = 0;
      }
      if (
        normalized.includes("low-pass") ||
        normalized.includes("low pass") ||
        normalized.includes("filter") ||
        (removeAll && automation.lowPass > 0)
      ) {
        automation.lowPass = 0;
        automation.filter = 0;
        automation.filterBypass = true;
      }
    }

    baseFilterForRole(role) {
      return {
        drums: 9000,
        bass: 1200,
        chords: 2800,
        lead: 3600,
        texture: 4200,
      }[role];
    }

    createReverbImpulse() {
      const length = Math.floor(this.context.sampleRate * 1.8);
      const impulse = this.context.createBuffer(2, length, this.context.sampleRate);
      const random = this.seededRandom(0x52455642);
      for (let channel = 0; channel < impulse.numberOfChannels; channel += 1) {
        const data = impulse.getChannelData(channel);
        for (let index = 0; index < length; index += 1) {
          const decay = (1 - index / length) ** 2.4;
          data[index] = (random() * 2 - 1) * decay;
        }
      }
      return impulse;
    }

    seededRandom(seed) {
      let value = seed >>> 0;
      return () => {
        value = (value + 0x6d2b79f5) >>> 0;
        let result = value;
        result = Math.imul(result ^ (result >>> 15), result | 1);
        result ^= result + Math.imul(result ^ (result >>> 7), result | 61);
        return ((result ^ (result >>> 14)) >>> 0) / 4294967296;
      };
    }

    stepDuration() {
      return 60 / state.project.bpm / 4;
    }

    updatePosition() {
      if (!this.isPlaying || !this.context) return;
      this.playhead = Math.min(
        state.project.duration,
        this.projectStartedAt + this.context.currentTime - this.contextStartedAt,
      );
    }

    animate() {
      if (!this.isPlaying) return;
      this.updatePosition();
      if (this.playhead >= state.project.duration) {
        this.stop(false);
        return;
      }
      updateTransport();
      renderPlayhead();
      this.frame = window.requestAnimationFrame(() => this.animate());
    }

    pump() {
      if (!this.isPlaying || !state.project) return;
      this.updatePosition();
      const scheduleUntil = this.playhead + 0.22;
      const stepDuration = this.stepDuration();
      while (this.nextStep <= scheduleUntil && this.nextStep < state.project.duration) {
        this.scheduleStep(this.nextStep, stepDuration);
        this.nextStep += stepDuration;
      }
    }

    scheduleStep(projectTime, stepDuration) {
      for (const track of state.project.tracks) {
        const clip = track.clips.find((candidate) => projectTime >= candidate.start && projectTime < candidate.end);
        if (track.muted || !clip) {
          continue;
        }
        const localStep = Math.floor((projectTime - clip.start) / stepDuration + 0.0001);
        const step = localStep % 16;
        const phraseTime = clip.start + localStep * stepDuration;
        const audioTime = Math.max(
          this.context.currentTime + 0.005,
          this.contextStartedAt + phraseTime - this.projectStartedAt,
        );
        const modifiers = this.modifiers(track, projectTime);

        if (track.role === "drums") {
          this.scheduleDrums(step, audioTime, 1, modifiers, track.id);
        } else if (track.role === "bass") {
          this.scheduleBass(step, audioTime, stepDuration, 1, modifiers, track.id);
        } else if (track.role === "chords") {
          this.scheduleChords(localStep, audioTime, stepDuration, 1, modifiers, track.id);
        } else if (track.role === "lead") {
          this.scheduleLead(step, audioTime, stepDuration, 1, modifiers, track.id);
        } else if (track.role === "texture") {
          this.scheduleTexture(localStep, audioTime, 1, modifiers, track.id);
        }
      }
    }

    chaseActiveVoices(stepDuration) {
      if (this.projectStartedAt <= 0) return;
      const audioTime = this.context.currentTime + 0.005;
      for (const track of state.project.tracks) {
        if (track.muted || track.role === "drums") continue;
        const clip = track.clips.find(
          (candidate) => this.projectStartedAt >= candidate.start && this.projectStartedAt < candidate.end,
        );
        if (!clip) continue;
        const lastLocalStep = Math.floor((this.projectStartedAt - clip.start) / stepDuration + 0.0001);
        for (let localStep = lastLocalStep; localStep >= 0; localStep -= 1) {
          const phraseTime = clip.start + localStep * stepDuration;
          const elapsed = this.projectStartedAt - phraseTime;
          if (elapsed <= 0.001) continue;
          if (elapsed > 3.5) break;
          const modifiers = this.modifiers(track, phraseTime);
          const step = localStep % 16;
          if (track.role === "bass") {
            this.scheduleBass(step, audioTime, stepDuration, 1, modifiers, track.id, elapsed);
          } else if (track.role === "chords") {
            this.scheduleChords(localStep, audioTime, stepDuration, 1, modifiers, track.id, elapsed);
          } else if (track.role === "lead") {
            this.scheduleLead(step, audioTime, stepDuration, 1, modifiers, track.id, elapsed);
          } else if (track.role === "texture") {
            this.scheduleTexture(localStep, audioTime, 1, modifiers, track.id, elapsed);
          }
        }
      }
    }

    modifiers(track, time) {
      const result = { rhythm: 0, release: 1, drop: false };
      for (const edit of state.project.edits) {
        if (time < edit.start || time >= edit.end) continue;
        this.applyPatternAction(edit.action, track.role, result);
      }
      return result;
    }

    applyPatternAction(action, role, result) {
      if (action.type === "compound") {
        for (const child of action.actions) this.applyPatternAction(child, role, result);
        return;
      }
      if (action.target !== "all" && action.target !== role) return;
      if (action.type === "rhythm") result.rhythm += action.value;
      if (action.type === "drop") {
        result.drop = true;
        result.rhythm += 0.55;
      }
    }

    rhythmInterval(baseInterval, modifiers) {
      if (modifiers.rhythm > 0.15) return Math.max(1, baseInterval / 2);
      if (modifiers.rhythm < -0.15) return baseInterval * 2;
      return baseInterval;
    }

    scheduleDrums(step, time, gain, modifiers, trackId) {
      const dense = modifiers.rhythm > 0.15 || modifiers.drop;
      const sparse = modifiers.rhythm < -0.15;
      if (step === 0 || step === 8 || (dense && (step === 6 || step === 14))) {
        this.kick(time, gain * 0.54, trackId);
      }
      if (step === 4 || step === 12) this.snare(time, gain * 0.25, trackId);
      if ((!sparse && step % 2 === 0) || (dense && step % 2 === 1)) {
        this.hat(time, gain * (step % 4 === 0 ? 0.085 : 0.055), trackId);
      }
    }

    scheduleBass(step, time, stepDuration, gain, modifiers, trackId, elapsed = 0) {
      const interval = this.rhythmInterval(4, modifiers);
      if (step % interval !== 0) return;
      const notes = [55, 55, 65.41, 49, 55, 73.42, 65.41, 49];
      const frequency = notes[Math.floor(step / 2) % notes.length];
      this.tone(frequency, time, stepDuration * Math.min(2.8, interval * 0.7), gain * 0.21, "square", modifiers, 850, trackId, elapsed);
    }

    scheduleChords(localStep, time, stepDuration, gain, modifiers, trackId, elapsed = 0) {
      const interval = this.rhythmInterval(8, modifiers);
      if (localStep % interval !== 0) return;
      const chordIndex = Math.floor(localStep / 8) % 4;
      const chords = [
        [220, 261.63, 329.63],
        [174.61, 220, 261.63],
        [196, 246.94, 293.66],
        [164.81, 207.65, 246.94],
      ];
      for (const frequency of chords[chordIndex]) {
        this.tone(frequency, time, stepDuration * Math.min(7.4, interval * 0.925), gain * 0.06, "triangle", modifiers, 1800, trackId, elapsed);
      }
    }

    scheduleLead(step, time, stepDuration, gain, modifiers, trackId, elapsed = 0) {
      const interval = this.rhythmInterval(4, modifiers);
      if (step % interval !== 0) return;
      const notes = [440, 523.25, 659.25, 587.33, 493.88, 440, 392, 493.88];
      const frequency = notes[Math.floor(step / 2) % notes.length];
      this.tone(frequency, time, stepDuration * 1.55, gain * 0.1, "sawtooth", modifiers, 2400, trackId, elapsed);
    }

    scheduleTexture(localStep, time, gain, modifiers, trackId, elapsed = 0) {
      if (localStep % this.rhythmInterval(16, modifiers) !== 0) return;
      this.tone(329.63, time, 2.6, gain * 0.035, "sine", { ...modifiers, release: 2.2 }, 3200, trackId, elapsed);
      this.tone(493.88, time, 2.2, gain * 0.022, "triangle", { ...modifiers, release: 2 }, 3600, trackId, elapsed);
    }

    tone(frequency, time, duration, level, type, modifiers, baseFilter, trackId, elapsed = 0) {
      const release = Math.max(0.04, Math.min(duration * modifiers.release, 3.5));
      const remaining = release - elapsed;
      if (remaining <= 0.01) return;
      const oscillator = this.context.createOscillator();
      const filter = this.context.createBiquadFilter();
      const envelope = this.context.createGain();
      oscillator.type = type;
      oscillator.dawAiTrackId = trackId;
      oscillator.dawAiChased = elapsed > 0;
      oscillator.frequency.setValueAtTime(frequency, time);
      filter.type = "lowpass";
      filter.frequency.value = baseFilter;
      if (elapsed > 0) {
        envelope.gain.setValueAtTime(Math.max(0.0002, level * (remaining / release)), time);
      } else {
        envelope.gain.setValueAtTime(0.0001, time);
        envelope.gain.exponentialRampToValueAtTime(Math.max(0.0002, level), time + 0.018);
      }
      envelope.gain.exponentialRampToValueAtTime(0.0001, time + remaining);
      oscillator.connect(filter);
      filter.connect(envelope);
      this.routeVoice(envelope, trackId);
      this.trackSource(oscillator);
      oscillator.start(time);
      oscillator.stop(time + remaining + 0.03);
    }

    routeVoice(output, trackId) {
      output.connect(this.trackGraphs.get(trackId).input);
    }

    trackSource(source) {
      this.activeSources.add(source);
      source.addEventListener("ended", () => this.activeSources.delete(source), { once: true });
    }

    kick(time, level, trackId) {
      const oscillator = this.context.createOscillator();
      const envelope = this.context.createGain();
      oscillator.dawAiTrackId = trackId;
      oscillator.frequency.setValueAtTime(145, time);
      oscillator.frequency.exponentialRampToValueAtTime(42, time + 0.16);
      envelope.gain.setValueAtTime(level, time);
      envelope.gain.exponentialRampToValueAtTime(0.0001, time + 0.2);
      oscillator.connect(envelope);
      this.routeVoice(envelope, trackId);
      this.trackSource(oscillator);
      oscillator.start(time);
      oscillator.stop(time + 0.21);
    }

    snare(time, level, trackId) {
      const source = this.context.createBufferSource();
      const filter = this.context.createBiquadFilter();
      const envelope = this.context.createGain();
      source.buffer = this.noiseBuffer;
      filter.type = "highpass";
      filter.frequency.value = 1100;
      envelope.gain.setValueAtTime(level, time);
      envelope.gain.exponentialRampToValueAtTime(0.0001, time + 0.12);
      source.connect(filter);
      filter.connect(envelope);
      this.routeVoice(envelope, trackId);
      this.trackSource(source);
      source.start(time);
      source.stop(time + 0.13);
    }

    hat(time, level, trackId) {
      const source = this.context.createBufferSource();
      const filter = this.context.createBiquadFilter();
      const envelope = this.context.createGain();
      source.buffer = this.noiseBuffer;
      filter.type = "highpass";
      filter.frequency.value = 6500;
      envelope.gain.setValueAtTime(level, time);
      envelope.gain.exponentialRampToValueAtTime(0.0001, time + 0.045);
      source.connect(filter);
      filter.connect(envelope);
      this.routeVoice(envelope, trackId);
      this.trackSource(source);
      source.start(time);
      source.stop(time + 0.05);
    }
  }

  const audio = new AudioEngine();

  async function api(path, options = {}) {
    const response = await fetch(path, options);
    const data = await response.json();
    if (!response.ok) throw new Error(data.error || "The studio could not complete that request.");
    return data;
  }

  async function loadProject() {
    try {
      state.project = await api("/api/project");
      renderProject();
    } catch (error) {
      showToast(error.message, true);
      elements.savedState.textContent = "Offline";
    }
  }

  function renderProject() {
    const project = state.project;
    if (!project) return;
    elements.projectName.textContent = project.name;
    elements.tempo.textContent = project.bpm;
    elements.totalTime.textContent = `/ ${formatTime(project.duration, false)}`;
    elements.savedState.textContent = `Version ${project.version}`;
    elements.undoButton.disabled = !project.canUndo;
    state.selectionStart = clamp(state.selectionStart, 0, project.duration - 0.25);
    state.selectionEnd = clamp(state.selectionEnd, state.selectionStart + 0.25, project.duration);
    renderRuler();
    renderTracks();
    renderSelection();
    renderPlayhead();
    renderEditLog();
    renderAdvanced();
    updateTransport();
    if (!state.centeredInitialSelection) {
      state.centeredInitialSelection = true;
      window.requestAnimationFrame(centerSelectionOnNarrowTimeline);
    }
  }

  function renderRuler() {
    const marks = [];
    const divisions = 16;
    for (let index = 0; index <= divisions; index += 1) {
      const time = (state.project.duration / divisions) * index;
      marks.push(`<span class="ruler-mark" style="left:${(index / divisions) * 100}%">${formatTime(time, false)}</span>`);
    }
    elements.rulerLane.innerHTML = marks.join("");
  }

  function renderTracks() {
    const duration = state.project.duration;
    elements.trackRows.innerHTML = state.project.tracks
      .map((track) => {
        const clips = track.clips
          .map((clip) => {
            const left = (clip.start / duration) * 100;
            const width = ((clip.end - clip.start) / duration) * 100;
            return `<div class="clip ${clip.style === "generated" ? "is-generated" : ""}" style="left:${left}%;width:${width}%;--track-color:${track.color}">
              <span class="clip-name">${escapeHtml(clip.label)}</span>
              <span class="waveform" aria-hidden="true">${waveformBars(track.id + clip.id)}</span>
            </div>`;
          })
          .join("");
        const markers = state.project.edits
          .filter((edit) => editAppliesToTrack(edit, track))
          .map((edit) => {
            const left = (edit.start / duration) * 100;
            const width = ((edit.end - edit.start) / duration) * 100;
            return `<span class="edit-marker" style="left:${left}%;width:${width}%" title="${escapeHtml(edit.summary)}"></span>`;
          })
          .join("");
        return `<div class="track-row" style="--track-color:${track.color}">
          <div class="track-label">
            <span class="track-color" aria-hidden="true"></span>
            <span class="track-meta"><strong>${escapeHtml(track.name)}</strong><span>${escapeHtml(track.role)}</span></span>
          </div>
          <div class="track-lane" data-track-id="${track.id}" role="slider" tabindex="0" aria-label="${escapeHtml(track.name)} timeline selection" aria-valuemin="0" aria-valuemax="${duration}" aria-valuenow="${state.selectionStart}" aria-valuetext="Selected ${state.selectionStart.toFixed(1)} to ${state.selectionEnd.toFixed(1)} seconds. Arrow keys move; Shift plus Arrow keys resize.">${clips}${markers}</div>
        </div>`;
      })
      .join("");
  }

  function renderSelection() {
    if (!state.project) return;
    const laneOffset = elements.rulerLane.offsetLeft;
    const laneWidth = elements.rulerLane.offsetWidth;
    const left = laneOffset + (state.selectionStart / state.project.duration) * laneWidth;
    const width = ((state.selectionEnd - state.selectionStart) / state.project.duration) * laneWidth;
    elements.selection.style.left = `${left}px`;
    elements.selection.style.width = `${Math.max(2, width)}px`;
    elements.selection.style.height = `${elements.trackRows.offsetHeight}px`;
    elements.selectionReadout.textContent = `${state.selectionStart.toFixed(1)}s - ${state.selectionEnd.toFixed(1)}s`;
    elements.promptRange.textContent = `${state.selectionStart.toFixed(1)} - ${state.selectionEnd.toFixed(1)} sec`;
    elements.trackRows.querySelectorAll(".track-lane").forEach((lane) => {
      lane.setAttribute("aria-valuenow", String(state.selectionStart));
      lane.setAttribute(
        "aria-valuetext",
        `Selected ${state.selectionStart.toFixed(1)} to ${state.selectionEnd.toFixed(1)} seconds`,
      );
    });
  }

  function renderPlayhead() {
    if (!state.project) return;
    const left = elements.rulerLane.offsetLeft + (audio.playhead / state.project.duration) * elements.rulerLane.offsetWidth;
    elements.playhead.style.left = `${left}px`;
  }

  function centerSelectionOnNarrowTimeline() {
    const scroll = elements.timelineScroll;
    if (scroll.scrollWidth <= scroll.clientWidth) return;
    const sidebarWidth = elements.rulerLane.offsetLeft;
    const availableWidth = scroll.clientWidth - sidebarWidth;
    const centerTime = (state.selectionStart + state.selectionEnd) / 2;
    const centerPosition = sidebarWidth + (centerTime / state.project.duration) * elements.rulerLane.offsetWidth;
    scroll.scrollLeft = Math.max(0, centerPosition - sidebarWidth - availableWidth / 2);
  }

  function renderEditLog() {
    const edits = state.project.edits;
    elements.editCount.textContent = `${edits.length} ${edits.length === 1 ? "edit" : "edits"}`;
    if (edits.length === 0) {
      elements.editLog.innerHTML = '<div class="empty-log">Select part of the timeline and ask Codex to shape it.</div>';
      return;
    }
    elements.editLog.innerHTML = [...edits]
      .reverse()
      .map(
        (edit, index) => `<article class="edit-item">
          <span class="edit-number">${edits.length - index}</span>
          <div><strong>${escapeHtml(edit.summary)}</strong><span class="edit-prompt">"${escapeHtml(edit.prompt)}"</span></div>
          <span class="edit-time">${edit.start.toFixed(1)} - ${edit.end.toFixed(1)}s</span>
        </article>`,
      )
      .join("");
  }

  function renderAdvanced() {
    elements.channelList.innerHTML = state.project.tracks
      .map((track) => {
        const baselineEffects = track.effects
          .map(
            (effect) => `<span class="effect-pill">${escapeHtml(effect.name)} <b>${Math.round(effect.mix * 100)}%</b></span>`,
          )
          .join("");
        const regionalEffects = regionalEffectsForTrack(track)
          .map((effect) => {
            return `<span class="effect-pill is-regional">${escapeHtml(effect.name)} <b>${escapeHtml(effect.detail)} &middot; ${effect.start.toFixed(1)}-${effect.end.toFixed(1)}s</b></span>`;
          })
          .join("");
        const effects = baselineEffects || regionalEffects ? baselineEffects + regionalEffects : '<span class="effect-pill">No effects</span>';
        return `<section class="channel-card" style="--track-color:${track.color}">
          <div class="channel-heading">
            <div class="channel-name"><i></i>${escapeHtml(track.name)}</div>
            <button class="mute-button ${track.muted ? "is-muted" : ""}" type="button" data-mute-track="${track.id}" data-muted="${track.muted}">${track.muted ? "MUTED" : "MUTE"}</button>
          </div>
          <label class="volume-control">LEVEL
            <input type="range" min="0" max="1.5" step="0.01" value="${track.volume}" data-volume-track="${track.id}" aria-label="${escapeHtml(track.name)} volume">
            <output>${Math.round(track.volume * 100)}%</output>
          </label>
          <div class="channel-details">
            <div class="detail-block"><span>Instrument</span><strong>${escapeHtml(track.instrument.engine)}</strong></div>
            <div class="detail-block"><span>Sound</span><strong>${escapeHtml(track.instrument.waveform)}</strong></div>
            <div class="detail-block"><span>Attack</span><strong>${escapeHtml(track.instrument.attack)}</strong></div>
            <div class="detail-block"><span>Release</span><strong>${escapeHtml(track.instrument.release)}</strong></div>
            <div class="effects-list">${effects}</div>
          </div>
        </section>`;
      })
      .join("");

    elements.channelList.querySelectorAll("[data-volume-track]").forEach((input) => {
      input.addEventListener("input", () => {
        input.nextElementSibling.value = `${Math.round(Number(input.value) * 100)}%`;
      });
      input.addEventListener("change", () => {
        void changeMix(input.dataset.volumeTrack, { volume: input.value }, "volume");
      });
    });
    elements.channelList.querySelectorAll("[data-mute-track]").forEach((button) => {
      button.addEventListener("click", () => {
        void changeMix(button.dataset.muteTrack, { muted: String(button.dataset.muted !== "true") }, "mute");
      });
    });
  }

  function regionalEffectsForTrack(track) {
    const effects = [];
    for (const edit of state.project.edits) collectRegionalEffects(edit.action, track.role, edit, effects);
    return effects;
  }

  function collectRegionalEffects(action, role, edit, effects) {
    if (action.type === "compound") {
      for (const child of action.actions) collectRegionalEffects(child, role, edit, effects);
      return;
    }
    if (action.type === "effect" && (action.target === "all" || action.target === role)) {
      effects.push({ name: action.name, detail: `${Math.round(action.value * 100)}%`, start: edit.start, end: edit.end });
    }
    if (action.type === "remove-effect" && (action.target === "all" || action.target === role)) {
      effects.push({ name: action.name, detail: "OFF", start: edit.start, end: edit.end });
    }
    if (action.type === "filter" && (action.target === "all" || action.target === role)) {
      const amount = Math.round(action.value * 100);
      effects.push({
        name: "Tone filter",
        detail: `${amount > 0 ? "+" : ""}${amount}%`,
        start: edit.start,
        end: edit.end,
      });
    }
  }

  async function changeMix(trackId, values, focusControl) {
    const resumePlayback = audio.isActive;
    audio.stop(true);
    try {
      state.project = await api("/api/mix", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({ track_id: trackId, ...values }),
      });
      renderProject();
      const selector =
        focusControl === "volume" ? `[data-volume-track="${trackId}"]` : `[data-mute-track="${trackId}"]`;
      elements.channelList.querySelector(selector)?.focus({ preventScroll: true });
    } catch (error) {
      showToast(error.message, true);
    } finally {
      if (resumePlayback && !audio.isActive) await audio.start();
    }
  }

  function editAppliesToTrack(edit, track) {
    return actionAppliesToTrack(edit.action, track.role);
  }

  function actionAppliesToTrack(action, role) {
    if (action.type === "compound") return action.actions.some((child) => actionAppliesToTrack(child, role));
    return action.target === "all" || action.target === role;
  }

  function waveformBars(seed) {
    let value = seed * 71 + 19;
    const bars = [];
    for (let index = 0; index < 44; index += 1) {
      value = (value * 16807) % 2147483647;
      const height = 12 + (value % 80);
      bars.push(`<i style="--bar-height:${height}%"></i>`);
    }
    return bars.join("");
  }

  function timelineTimeFromPointer(event) {
    const bounds = elements.rulerLane.getBoundingClientRect();
    const ratio = clamp((event.clientX - bounds.left) / bounds.width, 0, 1);
    return quantize(ratio * state.project.duration, 0.25);
  }

  function beginSelection(event) {
    if (!event.target.closest(".track-lane") || !state.project) return;
    if (event.pointerType === "touch" && !state.touchSelectionMode) return;
    state.dragPointer = event.pointerId;
    state.dragAnchor = Math.min(timelineTimeFromPointer(event), state.project.duration - 0.25);
    state.selectionStart = state.dragAnchor;
    state.selectionEnd = Math.min(state.project.duration, state.dragAnchor + 0.25);
    elements.trackRows.setPointerCapture(event.pointerId);
    renderSelection();
  }

  function moveSelection(event) {
    if (event.pointerId !== state.dragPointer) return;
    const current = timelineTimeFromPointer(event);
    state.selectionStart = Math.min(state.dragAnchor, current);
    state.selectionEnd = Math.max(state.dragAnchor, current);
    if (state.selectionEnd - state.selectionStart < 0.25) {
      state.selectionEnd = Math.min(state.project.duration, state.selectionStart + 0.25);
    }
    renderSelection();
  }

  function endSelection(event) {
    if (event.pointerId !== state.dragPointer) return;
    state.dragPointer = null;
    if (elements.trackRows.hasPointerCapture(event.pointerId)) {
      elements.trackRows.releasePointerCapture(event.pointerId);
    }
    audio.seek(state.selectionStart);
    renderSelection();
    if (event.pointerType === "touch") setTouchSelectionMode(false);
  }

  function setTouchSelectionMode(enabled) {
    state.touchSelectionMode = enabled;
    elements.trackRows.classList.toggle("is-touch-selecting", enabled);
    elements.selectionModeButton.setAttribute("aria-pressed", String(enabled));
    elements.selectionModeButton.textContent = enabled ? "Drag to select" : "Select region";
  }

  function handleTimelineKey(event) {
    if (!event.target.closest(".track-lane") || !state.project) return;
    const duration = state.project.duration;
    const width = state.selectionEnd - state.selectionStart;
    let handled = true;
    if (event.key === "Home") {
      state.selectionStart = 0;
      state.selectionEnd = width;
    } else if (event.key === "End") {
      state.selectionEnd = duration;
      state.selectionStart = duration - width;
    } else if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
      const change = event.key === "ArrowLeft" ? -0.25 : 0.25;
      if (event.shiftKey) {
        state.selectionEnd = clamp(state.selectionEnd + change, state.selectionStart + 0.25, duration);
      } else {
        state.selectionStart = clamp(state.selectionStart + change, 0, duration - width);
        state.selectionEnd = state.selectionStart + width;
      }
    } else {
      handled = false;
    }
    if (!handled) return;
    event.preventDefault();
    audio.seek(state.selectionStart);
    renderSelection();
  }

  async function submitPrompt(event) {
    event.preventDefault();
    if (state.promptPending) return;
    const prompt = elements.promptInput.value.trim();
    if (!prompt) return;
    state.promptPending = true;
    elements.composeButton.disabled = true;
    elements.composeButton.querySelector("span").textContent = "Codex is planning...";
    elements.savedState.textContent = "Waiting for Codex";
    try {
      audio.stop(true);
      const result = await api("/api/edits", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({
          prompt,
          start: String(state.selectionStart),
          end: String(state.selectionEnd),
        }),
      });
      state.project = result.project;
      elements.promptInput.value = "";
      renderProject();
      showToast(result.message);
    } catch (error) {
      showToast(error.message, true);
      elements.savedState.textContent = `Version ${state.project.version}`;
    } finally {
      state.promptPending = false;
      elements.composeButton.disabled = false;
      elements.composeButton.querySelector("span").textContent = "Make change";
    }
  }

  async function undo() {
    try {
      audio.stop(true);
      state.project = await api("/api/undo", { method: "POST" });
      renderProject();
      showToast("Last change undone");
    } catch (error) {
      showToast(error.message, true);
    }
  }

  async function reset() {
    if (!window.confirm("Reset to the original demo arrangement? You can still undo this.")) return;
    try {
      audio.stop(false);
      state.project = await api("/api/reset", { method: "POST" });
      state.selectionStart = 8;
      state.selectionEnd = 16;
      renderProject();
      showToast("Demo arrangement restored");
    } catch (error) {
      showToast(error.message, true);
    }
  }

  function openAdvanced() {
    window.clearTimeout(state.drawerHideTimer);
    elements.drawerBackdrop.hidden = false;
    elements.advancedDrawer.hidden = false;
    elements.advancedDrawer.inert = false;
    elements.advancedDrawer.setAttribute("aria-hidden", "false");
    elements.advancedButton.setAttribute("aria-expanded", "true");
    document.body.style.overflow = "hidden";
    window.requestAnimationFrame(() => elements.advancedDrawer.classList.add("is-open"));
    window.setTimeout(() => elements.closeAdvanced.focus(), 30);
  }

  function closeAdvanced() {
    elements.advancedDrawer.classList.remove("is-open");
    elements.advancedDrawer.inert = true;
    elements.advancedDrawer.setAttribute("aria-hidden", "true");
    elements.advancedButton.setAttribute("aria-expanded", "false");
    elements.drawerBackdrop.hidden = true;
    document.body.style.overflow = "";
    state.drawerHideTimer = window.setTimeout(() => {
      elements.advancedDrawer.hidden = true;
    }, 230);
    elements.advancedButton.focus();
  }

  function trapAdvancedFocus(event) {
    if (event.key !== "Tab") return;
    const focusable = [...elements.advancedDrawer.querySelectorAll("button, input, [href], [tabindex]")].filter(
      (element) => !element.disabled && element.tabIndex >= 0,
    );
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    const active = document.activeElement;
    if (event.shiftKey && (active === first || !elements.advancedDrawer.contains(active))) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && (active === last || !elements.advancedDrawer.contains(active))) {
      event.preventDefault();
      first.focus();
    }
  }

  function showToast(message, isError = false) {
    window.clearTimeout(state.toastTimer);
    elements.toast.textContent = message;
    elements.toast.classList.toggle("is-error", isError);
    elements.toast.hidden = false;
    state.toastTimer = window.setTimeout(() => {
      elements.toast.hidden = true;
    }, 4200);
  }

  function updateTransport() {
    elements.currentTime.textContent = formatTime(audio.playhead, true);
    elements.playButton.classList.toggle("is-playing", audio.isActive);
    elements.playButton.setAttribute("aria-label", audio.isActive ? "Pause project" : "Play project");
  }

  function formatTime(seconds, tenths) {
    const minutes = Math.floor(seconds / 60);
    const remainder = seconds - minutes * 60;
    return `${minutes}:${Math.floor(remainder).toString().padStart(2, "0")}${tenths ? `.${Math.floor((remainder % 1) * 10)}` : ""}`;
  }

  function clamp(value, minimum, maximum) {
    return Math.min(maximum, Math.max(minimum, value));
  }

  function quantize(value, amount) {
    return Math.round(value / amount) * amount;
  }

  function escapeHtml(value) {
    return String(value)
      .replaceAll("&", "&amp;")
      .replaceAll("<", "&lt;")
      .replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;")
      .replaceAll("'", "&#039;");
  }

  elements.trackRows.addEventListener("pointerdown", beginSelection);
  elements.trackRows.addEventListener("pointermove", moveSelection);
  elements.trackRows.addEventListener("pointerup", endSelection);
  elements.trackRows.addEventListener("pointercancel", endSelection);
  elements.trackRows.addEventListener("keydown", handleTimelineKey);
  elements.promptForm.addEventListener("submit", submitPrompt);
  elements.playButton.addEventListener("click", () => void audio.toggle());
  elements.rewindButton.addEventListener("click", () => audio.seek(0));
  elements.undoButton.addEventListener("click", () => void undo());
  elements.resetButton.addEventListener("click", () => void reset());
  elements.selectionModeButton.addEventListener("click", () => setTouchSelectionMode(!state.touchSelectionMode));
  elements.advancedButton.addEventListener("click", openAdvanced);
  elements.closeAdvanced.addEventListener("click", closeAdvanced);
  elements.drawerBackdrop.addEventListener("click", closeAdvanced);
  document.querySelectorAll("[data-prompt]").forEach((button) => {
    button.addEventListener("click", () => {
      elements.promptInput.value = button.dataset.prompt;
      elements.promptInput.focus();
    });
  });
  elements.promptInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      if (!state.promptPending) elements.promptForm.requestSubmit();
    }
  });
  window.addEventListener("resize", () => {
    renderSelection();
    renderPlayhead();
  });
  document.addEventListener("keydown", (event) => {
    if (elements.advancedDrawer.classList.contains("is-open")) {
      if (event.key === "Escape") closeAdvanced();
      trapAdvancedFocus(event);
    }
    if (event.code === "Space" && !event.target.matches("textarea, input, button")) {
      event.preventDefault();
      void audio.toggle();
    }
  });

  void loadProject();
})();
