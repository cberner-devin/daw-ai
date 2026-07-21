(() => {
  "use strict";

  const elements = {
    skipLink: document.querySelector(".skip-link"),
    projectName: document.querySelector("#project-name"),
    tempo: document.querySelector("#tempo"),
    currentTime: document.querySelector("#current-time"),
    totalTime: document.querySelector("#total-time"),
    playButton: document.querySelector("#play-button"),
    rewindButton: document.querySelector("#rewind-button"),
    timelinePanel: document.querySelector("#timeline-panel"),
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
    editProgress: document.querySelector("#edit-progress"),
    editProgressLabel: document.querySelector("#edit-progress-label"),
    editProgressTime: document.querySelector("#edit-progress-time"),
    editProgressTrack: document.querySelector("#edit-progress-track"),
    editProgressFill: document.querySelector("#edit-progress-fill"),
    undoButton: document.querySelector("#undo-button"),
    resetButton: document.querySelector("#reset-button"),
    savedState: document.querySelector("#saved-state"),
    editLog: document.querySelector("#edit-log"),
    editCount: document.querySelector("#edit-count"),
    aiModeButton: document.querySelector("#ai-mode-button"),
    aiModePanel: document.querySelector("#ai-mode-panel"),
    advancedButton: document.querySelector("#advanced-button"),
    closeAdvanced: document.querySelector("#close-advanced"),
    advancedDrawer: document.querySelector("#advanced-drawer"),
    channelCreator: document.querySelector("#channel-creator"),
    channelRole: document.querySelector("#channel-role"),
    addChannel: document.querySelector("#add-channel"),
    channelList: document.querySelector("#channel-list"),
    debugButton: document.querySelector("#debug-button"),
    debugPanel: document.querySelector("#debug-panel"),
    debugReport: document.querySelector("#debug-report"),
    copyDebug: document.querySelector("#copy-debug"),
    clearDebug: document.querySelector("#clear-debug"),
    refreshGeminiSessions: document.querySelector("#refresh-gemini-sessions"),
    geminiSessionList: document.querySelector("#gemini-session-list"),
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
    editProgressPercent: 0,
    channelMutationPending: false,
    centeredInitialSelection: false,
    toastTimer: null,
    activeView: "ai",
    clientIssues: [],
    geminiSessions: [],
  };

  let projectMutationQueue = Promise.resolve();
  const RECONCILED_REQUEST_TIMEOUT_MS = 2000;
  const EDIT_ACCEPTANCE_TIMEOUT_MS = 10_000;
  const PENDING_EDIT_STORAGE_KEY = "daw-ai.pending-edit.v1";

  class AudioEngine {
    constructor(project = null) {
      this.renderProject = project;
      this.context = null;
      this.master = null;
      this.playbackState = "idle";
      this.playbackGeneration = 0;
      this.playhead = 0;
      this.contextStartedAt = 0;
      this.projectStartedAt = 0;
      this.nextStep = 0;
      this.nextAutomationTime = 0;
      this.timer = null;
      this.frame = null;
      this.noiseBuffer = null;
      this.reverbImpulse = null;
      this.trackGraphs = new Map();
      this.modulatorPhaseCurves = new Map();
      this.activeSources = new Set();
    }

    get project() {
      return this.renderProject ?? state.project;
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
      if (!this.project || this.isActive) return;
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
        showError(error, "starting audio", "Could not start audio: ");
        return;
      }
      if (
        generation !== this.playbackGeneration ||
        this.playbackState !== "starting" ||
        this.context !== context
      ) {
        return;
      }
      if (this.playhead >= this.project.duration - 0.01) this.playhead = 0;

      this.createTrackGraphs();
      this.contextStartedAt = this.context.currentTime;
      this.projectStartedAt = this.playhead;
      this.nextAutomationTime = this.playhead;
      this.scheduleTrackExactBoundaries();
      this.playbackState = "playing";
      this.chaseActiveVoices();
      this.nextStep = this.playhead;
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
        this.modulatorPhaseCurves.clear();
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
      this.playhead = clamp(time, 0, this.project?.duration ?? 0);
      renderPlayhead();
      updateTransport();
      if (wasActive) void this.start();
    }

    createContext(context = null) {
      const AudioContext = window.AudioContext || window.webkitAudioContext;
      this.context = context ?? new AudioContext();
      const compressor = this.context.createDynamicsCompressor();
      compressor.threshold.value = -12;
      compressor.knee.value = 16;
      compressor.ratio.value = 5;
      this.master = this.context.createGain();
      this.master.gain.value = 0.58;
      this.master.connect(compressor);
      compressor.connect(this.context.destination);

      this.reverbImpulse = this.createReverbImpulse();
      this.driveCurve = this.createDriveCurve();

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

    async renderOffline(trackIds, start, end) {
      const selectedTrackIds = new Set(trackIds.map(Number));
      const project = structuredClone(this.project);
      for (const track of project.tracks) {
        if (!selectedTrackIds.has(track.id)) track.muted = true;
      }
      const sampleRate = 48_000;
      const frameCount = Math.ceil((end - start) * sampleRate);
      const renderer = new AudioEngine(project);
      renderer.createContext(new OfflineAudioContext(2, frameCount, sampleRate));
      renderer.createTrackGraphs();
      renderer.contextStartedAt = 0;
      renderer.projectStartedAt = start;
      renderer.nextAutomationTime = start;
      renderer.scheduleTrackExactBoundaries();
      renderer.chaseActiveVoices();
      renderer.scheduleTrackAutomation(start, end);
      renderer.scheduleWindow(start, end);
      return renderer.context.startRendering();
    }

    createTrackGraphs() {
      this.trackGraphs.clear();
      this.modulatorPhaseCurves.clear();
      for (const track of this.project.tracks) {
        for (const modulator of track.modulators) {
          this.modulatorPhaseCurves.set(
            `${track.id}:${modulator.id}`,
            this.buildModulatorPhaseCurve(track, modulator),
          );
        }
        const input = this.context.createGain();
        const toneFilter = this.context.createBiquadFilter();
        const effectFilter = this.context.createBiquadFilter();
        const filterOutput = this.context.createGain();
        const filterBypass = this.context.createGain();
        const chainInput = this.context.createGain();
        const gate = this.context.createGain();
        const echoSend = this.context.createGain();
        const delay = this.context.createDelay(2);
        const delayFeedback = this.context.createGain();
        const reverbSend = this.context.createGain();
        const reverb = this.context.createConvolver();
        const chorusSend = this.context.createGain();
        const chorusDelay = this.context.createDelay(0.05);
        const driveSend = this.context.createGain();
        const drive = this.context.createWaveShaper();
        const driveFilter = this.context.createBiquadFilter();
        const compressorSend = this.context.createGain();
        const compressor = this.context.createDynamicsCompressor();

        toneFilter.type = "lowpass";
        toneFilter.dawAiAutomation = "tone";
        toneFilter.dawAiTrackId = track.id;
        effectFilter.type = "lowpass";
        effectFilter.dawAiAutomation = "effect-filter";
        effectFilter.dawAiTrackId = track.id;
        effectFilter.Q.value = 0.7;
        filterOutput.gain.value = 0;
        filterBypass.gain.value = 0;
        gate.gain.value = 0;
        echoSend.gain.value = 0;
        reverbSend.gain.value = 0;
        chorusSend.gain.value = 0;
        driveSend.gain.value = 0;
        compressorSend.gain.value = 0;
        delay.delayTime.value = 60 / this.project.bpm / 2;
        delay.dawAiEffect = "echo";
        delayFeedback.gain.value = 0.24;
        reverb.buffer = this.reverbImpulse;
        chorusDelay.delayTime.value = 0.018;
        chorusDelay.dawAiEffect = "chorus";
        drive.curve = this.driveCurve;
        drive.oversample = "4x";
        drive.dawAiEffect = "drive";
        driveFilter.type = "highpass";
        driveFilter.frequency.value = 180;
        driveFilter.Q.value = 0.7;
        driveFilter.dawAiEffect = "drive-highpass";
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
        filterBypass.dawAiBypassedStage = effectFilter;
        echoSend.dawAiAutomation = "echo";
        echoSend.dawAiTrackId = track.id;
        reverbSend.dawAiAutomation = "reverb";
        reverbSend.dawAiTrackId = track.id;
        chorusSend.dawAiAutomation = "chorus";
        chorusSend.dawAiTrackId = track.id;
        driveSend.dawAiAutomation = "drive";
        driveSend.dawAiTrackId = track.id;
        compressorSend.dawAiAutomation = "compressor";
        compressorSend.dawAiTrackId = track.id;

        input.connect(toneFilter);
        toneFilter.connect(effectFilter);
        toneFilter.connect(filterBypass);
        effectFilter.connect(filterOutput);
        filterOutput.connect(chainInput);
        filterBypass.connect(chainInput);
        delay.connect(delayFeedback);
        delayFeedback.connect(delay);

        const stages = {
          echo: { send: echoSend, processors: [delay] },
          reverb: { send: reverbSend, processors: [reverb] },
          chorus: { send: chorusSend, processors: [chorusDelay] },
          drive: { send: driveSend, processors: [drive, driveFilter] },
          compressor: { send: compressorSend, processors: [compressor] },
        };
        const routedCategories = track.routing.audio
          .filter((node) => node.startsWith("effect:"))
          .map((node) => track.effects.find((effect) => effect.id === Number(node.slice(7))))
          .filter(Boolean)
          .map((effect) => this.effectCategory(effect.name))
          .filter(Boolean);
        const stageOrder = [
          ...new Set([...routedCategories, "drive", "echo", "reverb", "chorus", "compressor"]),
        ];
        let stageSource = chainInput;
        for (const category of stageOrder) {
          const { send, processors } = stages[category];
          const output = this.context.createGain();
          stageSource.connect(output);
          stageSource.connect(send);
          let processorOutput = send;
          for (const processor of processors) {
            processorOutput.connect(processor);
            processorOutput = processor;
          }
          processorOutput.connect(output);
          stageSource = output;
        }
        stageSource.connect(gate);
        gate.connect(this.master);
        this.trackGraphs.set(track.id, {
          input,
          toneFilter,
          effectFilter,
          filterOutput,
          filterBypass,
          gate,
          echoSend,
          reverbSend,
          chorusSend,
          driveSend,
          compressorSend,
        });
      }
    }

    scheduleTrackExactBoundaries() {
      for (const track of this.project.tracks) {
        const boundaries = new Set([this.projectStartedAt]);
        for (const edit of this.project.edits) {
          if (edit.start >= this.projectStartedAt) boundaries.add(edit.start);
          if (edit.end >= this.projectStartedAt) boundaries.add(edit.end);
          this.addExactActionBoundaries(
            edit.action,
            track.id,
            edit.start,
            edit.end,
            boundaries,
            this.projectStartedAt,
            this.project.duration,
          );
        }
        for (const clip of track.clips) {
          if (clip.start >= this.projectStartedAt) boundaries.add(clip.start);
          if (clip.end >= this.projectStartedAt) boundaries.add(clip.end);
        }
        const graph = this.trackGraphs.get(track.id);
        for (const boundary of [...boundaries].sort((left, right) => left - right)) {
          this.scheduleTrackBoundary(track, graph, boundary);
        }
      }
    }

    scheduleTrackAutomation(windowStart, windowEnd) {
      if (windowEnd <= windowStart) return;
      for (const track of this.project.tracks) {
        const graphModulators = track.modulators.filter(
          (modulator) => modulator.enabled && !this.isVoiceModulationTarget(modulator.target),
        );
        const hasAutomation = this.project.edits.some(
          (edit) => this.actionAutomatesTrack(edit.action, track.id),
        );
        const interval = Math.min(
          hasAutomation ? 0.01 : Number.POSITIVE_INFINITY,
          graphModulators.length > 0
            ? this.modulationInterval(graphModulators)
            : Number.POSITIVE_INFINITY,
        );
        if (!Number.isFinite(interval)) continue;
        const boundaries = new Set([windowStart, windowEnd]);
        const firstIndex = Math.ceil(
          (windowStart - this.projectStartedAt) / interval - 0.000001,
        );
        for (let index = Math.max(0, firstIndex); ; index += 1) {
          const time = this.projectStartedAt + index * interval;
          if (time > windowEnd + 0.000001) break;
          boundaries.add(time);
        }
        const orderedBoundaries = [...boundaries].sort((left, right) => left - right);
        const graph = this.trackGraphs.get(track.id);
        for (const boundary of orderedBoundaries) {
          this.scheduleTrackBoundary(track, graph, boundary);
        }
      }
    }

    scheduleTrackBoundary(track, graph, boundary) {
      const audioTime = Math.max(
        this.context.currentTime,
        this.contextStartedAt + boundary - this.projectStartedAt,
      );
      const automation = this.automationAt(track, boundary);
      graph.gate.gain.dawAiProjectTime = boundary;
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
      graph.driveSend.gain.setValueAtTime(Math.min(0.75, automation.drive * 0.75), audioTime);
      graph.compressorSend.gain.setValueAtTime(Math.min(0.5, automation.compression * 0.45), audioTime);
      const toneFrequency = clamp(
        this.baseFilterForRole(track.role) *
          (0.7 + automation.instrumentTone * 0.6) *
          (1 + automation.filter),
        180,
        9000,
      );
      graph.toneFilter.frequency.dawAiProjectTime = boundary;
      graph.toneFilter.frequency.setValueAtTime(toneFrequency, audioTime);
      graph.effectFilter.frequency.setValueAtTime(
        this.effectFilterFrequency(track, automation),
        audioTime,
      );
      graph.effectFilter.Q.setValueAtTime(automation.filterResonance, audioTime);
    }

    addExactActionBoundaries(
      action,
      trackId,
      start,
      end,
      boundaries,
      windowStart,
      windowEnd,
    ) {
      if (action.type === "compound") {
        for (const child of action.actions) {
          this.addExactActionBoundaries(
            child,
            trackId,
            start,
            end,
            boundaries,
            windowStart,
            windowEnd,
          );
        }
        return;
      }
      if (action.type === "timed") {
        const duration = end - start;
        const scopedStart = start + duration * action.start;
        const scopedEnd = start + duration * action.end;
        if (scopedStart >= windowStart && scopedStart <= windowEnd) boundaries.add(scopedStart);
        if (scopedEnd >= windowStart && scopedEnd <= windowEnd) boundaries.add(scopedEnd);
        this.addExactActionBoundaries(
          action.action,
          trackId,
          scopedStart,
          scopedEnd,
          boundaries,
          windowStart,
          windowEnd,
        );
        return;
      }
      if (
        action.type !== "automation" ||
        action.trackId !== trackId ||
        end < windowStart ||
        start > windowEnd
      ) return;
      for (const point of action.points) {
        const time = start + (end - start) * point.time;
        if (time >= windowStart && time <= windowEnd) boundaries.add(time);
      }
      if (start >= windowStart && start <= windowEnd) boundaries.add(start);
      if (end >= windowStart && end <= windowEnd) boundaries.add(end);
    }

    buildModulatorPhaseCurve(track, modulator) {
      const targetId = `modulator:${modulator.id}.rate`;
      const boundaries = new Set([0, this.project.duration]);
      for (const edit of this.project.edits) {
        this.collectRateAutomationBoundaries(
          edit.action,
          track.id,
          targetId,
          edit.start,
          edit.end,
          boundaries,
        );
      }
      const ordered = [...boundaries].sort((left, right) => left - right);
      const segments = [];
      let cumulativeCycles = 0;
      for (let index = 1; index < ordered.length; index += 1) {
        const start = clamp(ordered[index - 1], 0, this.project.duration);
        const end = clamp(ordered[index], 0, this.project.duration);
        const duration = end - start;
        if (duration <= 0.000001) continue;
        const firstTime = start + duration * 0.25;
        const secondTime = start + duration * 0.75;
        const firstRate = this.automatedParameterAt(
          track,
          targetId,
          modulator.parameters.rate,
          firstTime,
        );
        const secondRate = this.automatedParameterAt(
          track,
          targetId,
          modulator.parameters.rate,
          secondTime,
        );
        const slope = (secondRate - firstRate) / (secondTime - firstTime);
        const startRate = firstRate - slope * (firstTime - start);
        segments.push({ start, end, startRate, slope, cumulativeCycles });
        cumulativeCycles += startRate * duration + 0.5 * slope * duration ** 2;
      }
      return { segments, totalCycles: cumulativeCycles };
    }

    collectRateAutomationBoundaries(action, trackId, targetId, start, end, boundaries) {
      if (action.type === "compound") {
        for (const child of action.actions) {
          this.collectRateAutomationBoundaries(child, trackId, targetId, start, end, boundaries);
        }
        return;
      }
      if (action.type === "timed") {
        const duration = end - start;
        this.collectRateAutomationBoundaries(
          action.action,
          trackId,
          targetId,
          start + duration * action.start,
          start + duration * action.end,
          boundaries,
        );
        return;
      }
      if (action.type !== "automation" || action.trackId !== trackId || action.name !== targetId) return;
      boundaries.add(start);
      boundaries.add(end);
      for (const point of action.points) boundaries.add(start + (end - start) * point.time);
    }

    modulatorCyclesAt(track, modulator, time) {
      const curve = this.modulatorPhaseCurves.get(`${track.id}:${modulator.id}`);
      if (!curve) return Math.max(0, time) * modulator.parameters.rate;
      const segment = curve.segments.find((candidate) => time >= candidate.start && time <= candidate.end);
      if (!segment) return curve.totalCycles;
      const elapsed = clamp(time - segment.start, 0, segment.end - segment.start);
      return segment.cumulativeCycles + segment.startRate * elapsed + 0.5 * segment.slope * elapsed ** 2;
    }

    automationAt(track, time) {
      const clipActive = track.clips.some((clip) => time >= clip.start && time < clip.end);
      const automation = {
        gain: track.muted || !clipActive ? 0 : this.parameterAt(track, "track.volume", track.volume, time),
        instrumentTone: this.parameterAt(
          track,
          "instrument.tone",
          track.instrument.parameters.tone,
          time,
        ),
        filter: 0,
        filterBypass: false,
        lowPass: 0,
        filterCutoff: null,
        filterResonance: 0.7,
        echo: 0,
        reverb: {
          reverb: 0,
          room: 0,
          shimmer: 0,
        },
        chorus: 0,
        drive: 0,
        compression: 0,
      };
      for (const effect of track.effects) {
        if (!effect.enabled) continue;
        const target = `effect:${effect.id}.mix`;
        const mix = this.parameterAt(track, target, effect.parameters.mix, time);
        this.applyEffect(effect.name, mix, automation);
        if (Number.isFinite(effect.parameters.cutoff)) {
          automation.filterCutoff = this.parameterAt(
            track,
            `effect:${effect.id}.cutoff`,
            effect.parameters.cutoff,
            time,
          );
        }
        if (Number.isFinite(effect.parameters.resonance)) {
          automation.filterResonance = this.parameterAt(
            track,
            `effect:${effect.id}.resonance`,
            effect.parameters.resonance,
            time,
          );
        }
      }
      for (const edit of this.project.edits) {
        if (time >= edit.start && time < edit.end) {
          this.applyAutomationAction(edit.action, track.role, automation, edit, time);
        }
      }
      return automation;
    }

    modulatorValue(track, modulator, time) {
      let phaseOrigin = 0;
      if (modulator.trigger === "midi") {
        const onset = this.lastMidiOnset(track, time);
        if (onset === null) return 0;
        phaseOrigin = onset;
      }
      let cycles = this.modulatorCyclesAt(track, modulator, time) -
        this.modulatorCyclesAt(track, modulator, phaseOrigin);
      if (modulator.rateMode === "tempo") cycles *= this.project.bpm / 60;
      const phase = cycles * Math.PI * 2;
      let value;
      if (modulator.shape === "triangle") {
        value = (2 / Math.PI) * Math.asin(Math.sin(phase));
      } else if (modulator.shape === "square") {
        value = Math.sin(phase) >= 0 ? 1 : -1;
      } else if (modulator.shape === "envelope") {
        value = Math.abs(Math.sin(phase)) * 2 - 1;
      } else if (modulator.shape === "random") {
        value = Math.sin(Math.floor(cycles * 8) * 91.17 + modulator.id) * 0.8;
      } else {
        value = Math.sin(phase);
      }
      return value * this.automatedParameterAt(
        track,
        `modulator:${modulator.id}.depth`,
        modulator.parameters.depth,
        time,
      );
    }

    modulationInterval(modulators) {
      const fastestRate = Math.max(
        ...modulators.map((modulator) => {
          const target = `modulator:${modulator.id}.rate`;
          const automatedMaximum = Math.max(
            modulator.parameters.rate,
            ...this.project.edits.flatMap((edit) => this.automationPointValues(edit.action, target)),
          );
          return modulator.rateMode === "tempo"
            ? automatedMaximum * this.project.bpm / 60
            : automatedMaximum;
        }),
        0.01,
      );
      return clamp(1 / (fastestRate * 8), 0.0025, 0.025);
    }

    automationPointValues(action, targetId) {
      if (action.type === "compound") {
        return action.actions.flatMap((child) => this.automationPointValues(child, targetId));
      }
      if (action.type === "timed") return this.automationPointValues(action.action, targetId);
      return action.type === "automation" && action.name === targetId
        ? action.points.map((point) => point.value)
        : [];
    }

    actionAutomates(action, trackId, targetId) {
      if (action.type === "compound") {
        return action.actions.some((child) => this.actionAutomates(child, trackId, targetId));
      }
      if (action.type === "timed") return this.actionAutomates(action.action, trackId, targetId);
      return action.type === "automation" && action.trackId === trackId && action.name === targetId;
    }

    actionAutomatesTrack(action, trackId) {
      if (action.type === "compound") {
        return action.actions.some((child) => this.actionAutomatesTrack(child, trackId));
      }
      if (action.type === "timed") return this.actionAutomatesTrack(action.action, trackId);
      return action.type === "automation" && action.trackId === trackId;
    }

    hasParameterAutomation(track, targetId) {
      return this.project.edits.some((edit) => this.actionAutomates(edit.action, track.id, targetId));
    }

    isVoiceModulationTarget(target) {
      return ["instrument.attack", "instrument.release", "instrument.pitch"].includes(target) ||
        (target.startsWith("instrument.oscillator") &&
          (target.endsWith(".tuning") || target.endsWith(".level")));
    }

    lastMidiOnset(track, time) {
      const beatDuration = 60 / this.project.bpm;
      let latest = null;
      for (const clip of track.clips) {
        if (time < clip.start) continue;
        const loopDuration = clip.loopBeats * beatDuration;
        if (loopDuration <= 0) continue;
        const windowEnd = Math.min(time, clip.end) + 0.000002;
        const windowStart = Math.max(clip.start, windowEnd - loopDuration * 2);
        for (const occurrence of this.clipEventsInWindow(clip, track, windowStart, windowEnd)) {
          if (occurrence.time <= time && (latest === null || occurrence.time > latest)) {
            latest = occurrence.time;
          }
        }
      }
      return latest;
    }

    parameterAt(track, targetId, baseValue, time) {
      const target = (track.automationTargets || track.modulationTargets)
        .find((candidate) => candidate.id === targetId);
      if (!target) return baseValue;
      const automatedBase = this.automatedParameterAt(track, targetId, baseValue, time);
      const amount = track.modulators
        .filter((modulator) => modulator.enabled && modulator.target === targetId)
        .reduce((total, modulator) => total + this.modulatorValue(track, modulator, time), 0);
      const value = target.mode === "multiply"
        ? automatedBase * (1 + amount * target.scale)
        : target.mode === "exponential"
          ? automatedBase * 2 ** (amount * target.scale)
          : automatedBase + amount * target.scale;
      return clamp(value, target.minimum, target.maximum);
    }

    automatedParameterAt(track, targetId, baseValue, time) {
      let value = baseValue;
      for (const edit of this.project.edits) {
        const automated = this.automationActionValue(
          edit.action,
          track.id,
          targetId,
          time,
          edit.start,
          edit.end,
        );
        if (automated !== null) value = automated;
      }
      return value;
    }

    automationActionValue(action, trackId, targetId, time, start, end) {
      if (time < start || time >= end) return null;
      if (action.type === "compound") {
        let value = null;
        for (const child of action.actions) {
          const candidate = this.automationActionValue(child, trackId, targetId, time, start, end);
          if (candidate !== null) value = candidate;
        }
        return value;
      }
      if (action.type === "timed") {
        const duration = end - start;
        return this.automationActionValue(
          action.action,
          trackId,
          targetId,
          time,
          start + duration * action.start,
          start + duration * action.end,
        );
      }
      if (action.type !== "automation" || action.trackId !== trackId || action.name !== targetId) return null;
      const progress = clamp((time - start) / (end - start), 0, 1);
      if (action.curve === "hold") {
        return [...action.points].reverse().find((point) => point.time <= progress)?.value ?? action.points[0].value;
      }
      const upper = action.points.findIndex((point) => point.time >= progress);
      if (upper <= 0) return action.points[0].value;
      const previous = action.points[upper - 1];
      const next = action.points[upper];
      const amount = (progress - previous.time) / (next.time - previous.time);
      return previous.value + (next.value - previous.value) * amount;
    }

    instrumentParametersAt(track, time) {
      return {
        attack: this.parameterAt(track, "instrument.attack", track.instrument.parameters.attack, time),
        release: this.parameterAt(track, "instrument.release", track.instrument.parameters.release, time),
      };
    }

    applyAutomationAction(action, role, automation, edit, time) {
      if (action.type === "compound") {
        for (const child of action.actions) this.applyAutomationAction(child, role, automation, edit, time);
        return;
      }
      if (action.type === "timed") {
        const duration = edit.end - edit.start;
        const scopedEdit = {
          start: edit.start + duration * action.start,
          end: edit.start + duration * action.end,
        };
        if (time >= scopedEdit.start && time < scopedEdit.end) {
          this.applyAutomationAction(action.action, role, automation, scopedEdit, time);
        }
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
      if (normalized.includes("drive") || normalized.includes("distortion")) {
        automation.drive = Math.max(automation.drive, mix);
      }
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
      if (normalized.includes("drive") || normalized.includes("distortion") || removeAll) {
        automation.drive = 0;
      }
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

    effectFilterFrequency(track, automation) {
      const dryCutoff = 20000;
      const mix = clamp(automation.lowPass, 0, 1);
      if (mix === 0) return dryCutoff;
      const wetCutoff = clamp(
        automation.filterCutoff ?? this.baseFilterForRole(track.role) * 0.35,
        80,
        16000,
      );
      return dryCutoff * ((wetCutoff / dryCutoff) ** mix);
    }

    effectCategory(name) {
      const normalized = name.toLowerCase();
      if (normalized.includes("echo") || normalized.includes("delay")) return "echo";
      if (normalized === "reverb" || normalized === "room" || normalized === "shimmer") return "reverb";
      if (normalized.includes("chorus")) return "chorus";
      if (normalized.includes("drive") || normalized.includes("distortion")) return "drive";
      if (normalized.includes("compressor") || normalized.includes("compression")) return "compressor";
      return null;
    }

    createDriveCurve() {
      const curve = new Float32Array(4096);
      const normalization = Math.tanh(40);
      for (let index = 0; index < curve.length; index += 1) {
        const sample = (index * 2) / (curve.length - 1) - 1;
        curve[index] = Math.tanh(sample * 40) / normalization;
      }
      return curve;
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
      return 60 / this.project.bpm / 4;
    }

    updatePosition() {
      if (!this.isPlaying || !this.context) return;
      this.playhead = Math.min(
        this.project.duration,
        this.projectStartedAt + this.context.currentTime - this.contextStartedAt,
      );
    }

    animate() {
      if (!this.isPlaying) return;
      this.updatePosition();
      if (this.playhead >= this.project.duration) {
        this.stop(false);
        return;
      }
      updateTransport();
      renderPlayhead();
      this.frame = window.requestAnimationFrame(() => this.animate());
    }

    pump() {
      if (!this.isPlaying || !this.project) return;
      this.updatePosition();
      const scheduleUntil = Math.min(this.playhead + 0.22, this.project.duration);
      this.nextAutomationTime = Math.max(this.nextAutomationTime, this.playhead);
      this.scheduleTrackAutomation(this.nextAutomationTime, scheduleUntil);
      this.nextAutomationTime = scheduleUntil;
      const stepDuration = this.stepDuration();
      while (this.nextStep < scheduleUntil && this.nextStep < this.project.duration) {
        const windowEnd = Math.min(this.nextStep + stepDuration, this.project.duration);
        this.scheduleWindow(this.nextStep, windowEnd);
        this.nextStep = windowEnd;
      }
    }

    scheduleWindow(windowStart, windowEnd) {
      for (const track of this.project.tracks) {
        if (track.muted) continue;
        for (const clip of track.clips) {
          if (clip.end <= windowStart || clip.start >= windowEnd) continue;
          for (const occurrence of this.clipEventsInWindow(clip, track, windowStart, windowEnd)) {
            const audioTime = Math.max(
              this.context.currentTime + 0.005,
              this.contextStartedAt + occurrence.time - this.projectStartedAt,
            );
            this.scheduleClipEvent(
              occurrence.event,
              track,
              audioTime,
              0,
              occurrence.time,
            );
          }
        }
      }
    }

    clipEventsInWindow(clip, track, windowStart, windowEnd) {
      const groups = new Map();
      for (const event of clip.events) {
        if (!groups.has(event.time)) groups.set(event.time, []);
        groups.get(event.time).push(event);
      }
      const onsets = [...groups.keys()].sort((left, right) => left - right);
      const pattern = onsets.flatMap((onset, onsetIndex) =>
        groups.get(onset).map((event) => ({ event, onsetIndex, densityEvent: false })),
      );
      for (let index = 0; index < onsets.length; index += 1) {
        const previous = onsets[index];
        const next = index + 1 < onsets.length ? onsets[index + 1] : onsets[0] + clip.loopBeats;
        const gap = next - previous;
        if (gap < 0.5) continue;
        const midpoint = (previous + gap / 2) % clip.loopBeats;
        if (onsets.some((onset) => Math.abs(onset - midpoint) < 0.000001)) continue;
        for (const event of groups.get(previous)) {
          pattern.push({
            event: {
              ...event,
              time: midpoint,
              duration: Math.max(0.0625, event.duration * 0.7),
              velocity: Math.max(0.01, event.velocity * 0.82),
            },
            onsetIndex: index,
            densityEvent: true,
          });
        }
      }

      const beatDuration = 60 / this.project.bpm;
      const loopDuration = clip.loopBeats * beatDuration;
      const sourceStart = clip.sourceStart ?? clip.start;
      const firstCycle = Math.max(0, Math.floor((windowStart - sourceStart) / loopDuration) - 1);
      const lastCycle = Math.max(0, Math.floor((windowEnd - sourceStart) / loopDuration));
      const occurrences = [];
      for (let cycle = firstCycle; cycle <= lastCycle; cycle += 1) {
        for (const candidate of pattern) {
          const time = sourceStart + cycle * loopDuration + candidate.event.time * beatDuration;
          if (time < clip.start || time >= clip.end) continue;
          if (time < windowStart - 0.000001 || time >= windowEnd - 0.000001) continue;
          const modifiers = this.modifiers(track, time);
          if (candidate.densityEvent && modifiers.rhythm <= 0.15) continue;
          if (
            !candidate.densityEvent &&
            modifiers.rhythm < -0.15 &&
            (cycle * onsets.length + candidate.onsetIndex) % 2 !== 0
          ) continue;
          occurrences.push({ event: candidate.event, time });
        }
      }
      return occurrences.sort((left, right) => left.time - right.time);
    }

    scheduleClipEvent(event, track, time, elapsed, projectTime, onsetTime = projectTime) {
      const velocity = clamp(event.velocity, 0.01, 1);
      const instrument = this.instrumentParametersAt(track, onsetTime);
      if (track.role === "drums" || event.type !== "note") {
        const drumEvent = event.type === "note"
          ? { ...event, type: this.drumTypeForPitch(event.pitch) }
          : event;
        this.drum(drumEvent, track, time, projectTime, elapsed, instrument);
        return;
      }

      const beatDuration = 60 / this.project.bpm;
      const frequency = 440 * 2 ** ((event.pitch - 69) / 12);
      const roleLevel = { bass: 0.24, chords: 0.09, lead: 0.13, texture: 0.07 }[track.role] ?? 0.1;
      this.tone(
        frequency,
        time,
        event.duration * beatDuration,
        velocity * roleLevel,
        track.id,
        elapsed,
        instrument,
        track,
        projectTime,
        event,
      );
    }

    chaseActiveVoices() {
      if (this.projectStartedAt <= 0) return;
      const audioTime = this.context.currentTime + 0.005;
      for (const track of this.project.tracks) {
        if (track.muted) continue;
        const beatDuration = 60 / this.project.bpm;
        const longestEvent = Math.max(
          ...track.clips.flatMap((clip) => clip.events.map((event) => event.duration * beatDuration)),
          0,
        );
        const maximumRelease = track.modulationTargets.find(
          (target) => target.id === "instrument.release",
        )?.maximum ?? track.instrument.parameters.release;
        const lookback = Math.max(0, this.projectStartedAt - longestEvent - maximumRelease);
        for (const clip of track.clips) {
          if (clip.end <= lookback || clip.start >= this.projectStartedAt) continue;
          for (const occurrence of this.clipEventsInWindow(clip, track, lookback, this.projectStartedAt)) {
            const elapsed = this.projectStartedAt - occurrence.time;
            if (elapsed <= 0.001) continue;
            const event = occurrence.event;
            const instrument = this.instrumentParametersAt(track, occurrence.time);
            const soundingFor = event.duration * beatDuration + instrument.release;
            if (elapsed < soundingFor) {
              this.scheduleClipEvent(
                event,
                track,
                audioTime,
                elapsed,
                this.projectStartedAt,
                occurrence.time,
              );
            }
          }
        }
      }
    }

    modifiers(track, time) {
      const result = { rhythm: 0 };
      for (const edit of this.project.edits) {
        if (time < edit.start || time >= edit.end) continue;
        this.applyPatternAction(edit.action, track.role, result, edit, time);
      }
      return result;
    }

    applyPatternAction(action, role, result, edit, time) {
      if (action.type === "compound") {
        for (const child of action.actions) this.applyPatternAction(child, role, result, edit, time);
        return;
      }
      if (action.type === "timed") {
        const duration = edit.end - edit.start;
        const scopedEdit = {
          start: edit.start + duration * action.start,
          end: edit.start + duration * action.end,
        };
        if (time >= scopedEdit.start && time < scopedEdit.end) {
          this.applyPatternAction(action.action, role, result, scopedEdit, time);
        }
        return;
      }
      if (action.target !== "all" && action.target !== role) return;
      if (action.type === "rhythm") result.rhythm += action.value;
    }

    drumTypeForPitch(pitch) {
      if (pitch === 35 || pitch === 36) return "kick";
      if (pitch >= 37 && pitch <= 40) return "snare";
      if ([41, 43, 45, 47, 48, 50].includes(pitch)) return "tom";
      if ([42, 44, 46].includes(pitch)) return "hat";
      if ([49, 51, 52, 53, 55, 57, 59].includes(pitch)) return "cymbal";
      return "percussion";
    }

    scheduleOscillatorTuning(parameter, track, oscillator, oscillatorIndex, audioTime, projectTime, duration) {
      const tuningTarget = `instrument.oscillator${oscillatorIndex + 1}.tuning`;
      const modulators = track.modulators.filter((modulator) =>
        modulator.enabled && ["instrument.pitch", tuningTarget].includes(modulator.target),
      );
      const automated = this.hasParameterAutomation(track, "instrument.pitch") ||
        this.hasParameterAutomation(track, tuningTarget);
      if (modulators.length === 0 && !automated) return;
      const interval = modulators.length === 0 ? 0.025 : this.modulationInterval(modulators);
      for (let offset = 0; offset <= duration; offset += interval) {
        const semitones =
          this.parameterAt(track, "instrument.pitch", 0, projectTime + offset) +
          this.parameterAt(track, tuningTarget, oscillator.tuning, projectTime + offset);
        parameter.setValueAtTime(semitones * 100, audioTime + offset);
      }
    }

    scheduleOscillatorLevel(parameter, track, oscillator, oscillatorIndex, audioTime, projectTime, duration) {
      const target = `instrument.oscillator${oscillatorIndex + 1}.level`;
      const modulators = track.modulators.filter(
        (modulator) => modulator.enabled && modulator.target === target,
      );
      const automated = this.hasParameterAutomation(track, target);
      if (modulators.length === 0 && !automated) {
        parameter.setValueAtTime(oscillator.level, audioTime);
        return;
      }
      const interval = modulators.length === 0 ? 0.025 : this.modulationInterval(modulators);
      for (let offset = 0; offset <= duration; offset += interval) {
        parameter.setValueAtTime(
          this.parameterAt(track, target, oscillator.level, projectTime + offset),
          audioTime + offset,
        );
      }
    }

    drum(event, track, time, projectTime, elapsed, instrument) {
      const beatDuration = 60 / this.project.bpm;
      const bodyDuration = Math.max(0.01, event.duration * beatDuration);
      const totalDuration = bodyDuration + instrument.release;
      const remaining = totalDuration - elapsed;
      if (remaining <= 0.01) return;
      const attack = instrument.attack;
      const velocity = clamp(event.velocity, 0.01, 1);
      const frequency = 440 * 2 ** ((event.pitch - 69) / 12);
      const tonalEnvelope = this.context.createGain();
      const tonalLevel = velocity * ({
        kick: 0.58,
        snare: 0.055,
        tom: 0.34,
        hat: 0.028,
        cymbal: 0.012,
        percussion: 0.11,
      }[event.type] ?? 0.05);
      this.scheduleVoiceEnvelope(
        tonalEnvelope.gain,
        time,
        attack,
        bodyDuration,
        totalDuration,
        tonalLevel,
        elapsed,
      );
      tonalEnvelope.dawAiVoiceEnvelope = true;
      tonalEnvelope.dawAiVoiceKind = "tonal";
      tonalEnvelope.dawAiTrackId = track.id;
      tonalEnvelope.dawAiEventId = event.id;
      this.routeVoice(tonalEnvelope, track.id);
      track.instrument.oscillators.forEach((oscillatorConfig, oscillatorIndex) => {
        const oscillator = this.context.createOscillator();
        const oscillatorLevel = this.context.createGain();
        oscillatorLevel.dawAiAutomation = "oscillator-level";
        oscillatorLevel.dawAiTrackId = track.id;
        oscillatorLevel.dawAiOscillatorIndex = oscillatorIndex;
        oscillator.type = oscillatorConfig.waveform;
        oscillator.dawAiTrackId = track.id;
        oscillator.dawAiChased = elapsed > 0;
        oscillator.dawAiDrumType = event.type;
        oscillator.dawAiEventId = event.id;
        oscillator.dawAiEventPitch = event.pitch;
        oscillator.dawAiEventDuration = event.duration;
        oscillator.dawAiInstrumentAttack = instrument.attack;
        oscillator.dawAiInstrumentRelease = instrument.release;
        oscillator.dawAiOscillatorIndex = oscillatorIndex;
        if (event.type === "kick" || event.type === "tom") {
          const startFrequency = frequency * (event.type === "kick" ? 3.2 : 1.8);
          const endFrequency = Math.max(
            event.type === "kick" ? 20 : 35,
            frequency * (event.type === "kick" ? 1 : 0.78),
          );
          if (elapsed < bodyDuration) {
            const progress = elapsed / bodyDuration;
            const currentFrequency = startFrequency * (endFrequency / startFrequency) ** progress;
            oscillator.frequency.setValueAtTime(currentFrequency, time);
            oscillator.frequency.exponentialRampToValueAtTime(
              endFrequency,
              time + bodyDuration - elapsed,
            );
          } else {
            oscillator.frequency.setValueAtTime(endFrequency, time);
          }
        } else {
          oscillator.frequency.setValueAtTime(frequency, time);
        }
        oscillator.detune.setValueAtTime(oscillatorConfig.tuning * 100, time);
        this.scheduleOscillatorTuning(
          oscillator.detune,
          track,
          oscillatorConfig,
          oscillatorIndex,
          time,
          projectTime,
          remaining,
        );
        this.scheduleOscillatorLevel(
          oscillatorLevel.gain,
          track,
          oscillatorConfig,
          oscillatorIndex,
          time,
          projectTime,
          remaining,
        );
        oscillator.connect(oscillatorLevel);
        oscillatorLevel.connect(tonalEnvelope);
        this.trackSource(oscillator);
        oscillator.start(time);
        oscillator.stop(time + remaining + 0.01);
      });

      if (event.type === "kick" || event.type === "tom") return;
      const source = this.context.createBufferSource();
      const filter = this.context.createBiquadFilter();
      const noiseEnvelope = this.context.createGain();
      source.buffer = this.noiseBuffer;
      source.loop = true;
      source.dawAiTrackId = track.id;
      source.dawAiChased = elapsed > 0;
      source.dawAiDrumType = event.type;
      source.dawAiEventPitch = event.pitch;
      source.dawAiEventDuration = event.duration;
      filter.type = "highpass";
      const noise = {
        snare: { multiplier: 24, minimum: 300, level: 0.3 },
        hat: { multiplier: 60, minimum: 3000, level: 0.1 },
        cymbal: { multiplier: 48, minimum: 3500, level: 0.22 },
        percussion: { multiplier: 32, minimum: 800, level: 0.12 },
      }[event.type];
      filter.frequency.value = clamp(frequency * noise.multiplier, noise.minimum, 12000);
      const noiseLevel = velocity * noise.level;
      this.scheduleVoiceEnvelope(
        noiseEnvelope.gain,
        time,
        attack,
        bodyDuration,
        totalDuration,
        noiseLevel,
        elapsed,
      );
      noiseEnvelope.dawAiVoiceEnvelope = true;
      noiseEnvelope.dawAiVoiceKind = "noise";
      noiseEnvelope.dawAiTrackId = track.id;
      noiseEnvelope.dawAiEventId = event.id;
      source.connect(filter);
      filter.connect(noiseEnvelope);
      this.routeVoice(noiseEnvelope, track.id);
      this.trackSource(source);
      source.start(time);
      source.stop(time + remaining + 0.01);
    }

    scheduleVoiceEnvelope(parameter, time, attack, bodyDuration, totalDuration, level, elapsed) {
      const peak = Math.max(0.0002, level);
      const floor = 0.0001;
      const remaining = totalDuration - elapsed;
      const attackDuration = Math.max(0.001, attack);
      const attackEnd = Math.min(attackDuration, bodyDuration);
      const levelDuringAttack = (offset) =>
        floor * (peak / floor) ** clamp(offset / attackDuration, 0, 1);
      const noteOffLevel = levelDuringAttack(attackEnd);
      if (elapsed < bodyDuration) {
        parameter.setValueAtTime(
          levelDuringAttack(Math.min(elapsed, attackDuration)),
          time,
        );
        if (elapsed < attackEnd) {
          parameter.exponentialRampToValueAtTime(
            noteOffLevel,
            time + attackEnd - elapsed,
          );
        }
        if (bodyDuration > attackEnd) {
          parameter.setValueAtTime(noteOffLevel, time + bodyDuration - elapsed);
        }
      } else {
        const releaseDuration = Math.max(0.001, totalDuration - bodyDuration);
        const progress = clamp((elapsed - bodyDuration) / releaseDuration, 0, 1);
        const currentLevel = noteOffLevel * (floor / noteOffLevel) ** progress;
        parameter.setValueAtTime(currentLevel, time);
      }
      parameter.exponentialRampToValueAtTime(floor, time + remaining);
    }

    tone(
      frequency,
      time,
      duration,
      level,
      trackId,
      elapsed,
      instrument,
      track,
      projectTime,
      event,
    ) {
      const soundingDuration = duration + instrument.release;
      const remaining = soundingDuration - elapsed;
      if (remaining <= 0.01) return;
      const envelope = this.context.createGain();
      this.scheduleVoiceEnvelope(
        envelope.gain,
        time,
        instrument.attack,
        duration,
        soundingDuration,
        level,
        elapsed,
      );
      envelope.dawAiVoiceEnvelope = true;
      envelope.dawAiTrackId = trackId;
      envelope.dawAiEventId = event.id;
      this.routeVoice(envelope, trackId);
      track.instrument.oscillators.forEach((oscillatorConfig, oscillatorIndex) => {
        const oscillator = this.context.createOscillator();
        const oscillatorLevel = this.context.createGain();
        oscillatorLevel.dawAiAutomation = "oscillator-level";
        oscillatorLevel.dawAiTrackId = track.id;
        oscillatorLevel.dawAiOscillatorIndex = oscillatorIndex;
        oscillator.type = oscillatorConfig.waveform;
        oscillator.dawAiTrackId = trackId;
        oscillator.dawAiChased = elapsed > 0;
        oscillator.dawAiBaseFrequency = frequency;
        oscillator.dawAiEventId = event.id;
        oscillator.dawAiEventTime = event.time;
        oscillator.dawAiProjectTime = projectTime;
        oscillator.dawAiEventDuration = event.duration;
        oscillator.dawAiInstrumentAttack = instrument.attack;
        oscillator.dawAiInstrumentRelease = instrument.release;
        oscillator.dawAiOscillatorIndex = oscillatorIndex;
        oscillator.frequency.setValueAtTime(frequency, time);
        oscillator.detune.setValueAtTime(oscillatorConfig.tuning * 100, time);
        this.scheduleOscillatorTuning(
          oscillator.detune,
          track,
          oscillatorConfig,
          oscillatorIndex,
          time,
          projectTime,
          remaining,
        );
        this.scheduleOscillatorLevel(
          oscillatorLevel.gain,
          track,
          oscillatorConfig,
          oscillatorIndex,
          time,
          projectTime,
          remaining,
        );
        oscillator.connect(oscillatorLevel);
        oscillatorLevel.connect(envelope);
        this.trackSource(oscillator);
        oscillator.start(time);
        oscillator.stop(time + remaining + 0.03);
      });
    }

    routeVoice(output, trackId) {
      output.connect(this.trackGraphs.get(trackId).input);
    }

    trackSource(source) {
      this.activeSources.add(source);
      source.addEventListener("ended", () => this.activeSources.delete(source), { once: true });
    }

  }

  const audio = new AudioEngine();

  function wavBytes(buffer) {
    const channels = buffer.numberOfChannels;
    const frameCount = buffer.length;
    const bytesPerFrame = channels * 2;
    const dataSize = frameCount * bytesPerFrame;
    const bytes = new Uint8Array(44 + dataSize);
    const view = new DataView(bytes.buffer);
    const writeText = (offset, value) => {
      for (let index = 0; index < value.length; index += 1) {
        view.setUint8(offset + index, value.charCodeAt(index));
      }
    };
    writeText(0, "RIFF");
    view.setUint32(4, 36 + dataSize, true);
    writeText(8, "WAVE");
    writeText(12, "fmt ");
    view.setUint32(16, 16, true);
    view.setUint16(20, 1, true);
    view.setUint16(22, channels, true);
    view.setUint32(24, buffer.sampleRate, true);
    view.setUint32(28, buffer.sampleRate * bytesPerFrame, true);
    view.setUint16(32, bytesPerFrame, true);
    view.setUint16(34, 16, true);
    writeText(36, "data");
    view.setUint32(40, dataSize, true);
    const channelData = Array.from(
      { length: channels },
      (_unused, channel) => buffer.getChannelData(channel),
    );
    let offset = 44;
    for (let frame = 0; frame < frameCount; frame += 1) {
      for (let channel = 0; channel < channels; channel += 1) {
        const sample = clamp(channelData[channel][frame], -1, 1);
        view.setInt16(offset, Math.round(sample * (sample < 0 ? 32768 : 32767)), true);
        offset += 2;
      }
    }
    return bytes;
  }

  function bytesBase64(bytes) {
    let binary = "";
    for (let offset = 0; offset < bytes.length; offset += 0x8000) {
      binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
    }
    return window.btoa(binary);
  }

  function validateAudioRenderRequest(request) {
    if (!request || !/^[A-Za-z0-9_-]{1,128}$/.test(String(request.id))) {
      throw new Error("The studio sent an invalid audio render ID.");
    }
    if (!request.project || !Array.isArray(request.project.tracks)) {
      throw new Error("The studio sent an invalid audio render project.");
    }
    if (!Array.isArray(request.trackIds) || request.trackIds.length === 0) {
      throw new Error("The studio sent no channels to render.");
    }
    if (
      !Number.isFinite(request.start) ||
      !Number.isFinite(request.end) ||
      request.start < 0 ||
      request.end <= request.start ||
      request.end > request.project.duration
    ) {
      throw new Error("The studio sent an invalid audio render range.");
    }
  }

  async function renderServerAudio(token) {
    const request = await api(`/api/server-audio-renders/${encodeURIComponent(token)}`);
    let body;
    try {
      validateAudioRenderRequest(request);
      const buffer = await new AudioEngine(request.project).renderOffline(
        request.trackIds,
        request.start,
        request.end,
      );
      body = { wav: bytesBase64(wavBytes(buffer)) };
    } catch (error) {
      reportClientIssue("error", error, "rendering audio for Gemini");
      body = { error: errorMessage(error).slice(0, 500) };
    }
    await api(
      `/api/server-audio-renders/${encodeURIComponent(token)}`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      },
      30_000,
    );
    document.documentElement.dataset.serverAudioRender = body.wav ? "completed" : "failed";
  }

  window.__dawAiRenderAudio = async (project, trackIds, start, end) => {
    const buffer = await new AudioEngine(project).renderOffline(trackIds, start, end);
    return wavBytes(buffer);
  };

  class ApiError extends Error {
    constructor(message, status, retryable) {
      super(message);
      this.name = "ApiError";
      this.status = status;
      this.retryable = retryable;
    }
  }

  class CommittedEditSyncError extends Error {
    constructor(cause) {
      super(`The edit completed, but the project could not be refreshed. Reload to see it. ${errorMessage(cause)}`);
      this.name = "CommittedEditSyncError";
    }
  }

  function isRetryableHttpStatus(status) {
    return status >= 500 || status === 408 || status === 429;
  }

  async function api(path, options = {}, timeoutMs = null) {
    let requestOptions = options;
    let timeout = null;
    if (timeoutMs !== null) {
      const controller = new AbortController();
      timeout = window.setTimeout(() => controller.abort(), Math.max(1, timeoutMs));
      requestOptions = { ...options, signal: controller.signal };
    }
    try {
      const response = await fetch(path, requestOptions);
      let data;
      try {
        data = await response.json();
      } catch (_error) {
        throw new ApiError(
          `The studio returned an invalid response (${response.status}).`,
          response.status,
          response.ok || isRetryableHttpStatus(response.status),
        );
      }
      if (!response.ok) {
        throw new ApiError(
          data.error || "The studio could not complete that request.",
          response.status,
          isRetryableHttpStatus(response.status),
        );
      }
      return data;
    } finally {
      if (timeout !== null) window.clearTimeout(timeout);
    }
  }

  function isRetryableApiError(error) {
    return !(error instanceof ApiError) || error.retryable;
  }

  function errorMessage(error) {
    if (error instanceof Error && error.message) return error.message;
    if (typeof error === "string" && error) return error;
    return "Unknown browser error";
  }

  function reportClientIssue(level, error, context) {
    const message = errorMessage(error);
    const stack = error instanceof Error && error.stack ? `\n${error.stack}` : "";
    state.clientIssues.push({
      time: new Date().toISOString(),
      level,
      context: String(context || "browser").slice(0, 160),
      message: `${message}${stack}`.slice(0, 4096),
    });
    state.clientIssues = state.clientIssues.slice(-20);
    renderDebug();
    const body = new URLSearchParams({
      level,
      context: String(context || "browser").slice(0, 160),
      message: `${message}${stack}`.slice(0, 4096),
    });
    void fetch("/api/logs", {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body,
      keepalive: true,
    }).catch(() => {});
  }

  function showError(error, context, prefix = "") {
    reportClientIssue("error", error, context);
    showToast(prefix + errorMessage(error), true);
  }

  async function loadProject() {
    try {
      state.project = await api("/api/project");
      renderProject();
    } catch (error) {
      showError(error, "loading the project");
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
    renderDebug();
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
      elements.editLog.innerHTML = '<div class="empty-log">Select part of the timeline and ask Gemini to shape it.</div>';
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
    const uiState = captureAdvancedUiState();
    elements.channelList.innerHTML = state.project.tracks
      .map((track) => {
        const regionalEffects = regionalEffectsForTrack(track)
          .map((effect) => {
            return `<span class="effect-pill is-regional">${escapeHtml(effect.name)} <b>${escapeHtml(effect.detail)} &middot; ${effect.start.toFixed(1)}-${effect.end.toFixed(1)}s</b></span>`;
          })
          .join("");
        const orderedEffects = track.routing.audio
          .filter((node) => node.startsWith("effect:"))
          .map((node) => track.effects.find((effect) => effect.id === Number(node.slice(7))))
          .filter(Boolean);
        const routeNodes = ["MIDI Clips", "Instrument", ...orderedEffects.map((effect) => effect.name), "Master"];
        const route = routeNodes
          .map((node, index) => {
            const signal = index === 0 ? "MIDI" : "AUDIO";
            const edge = index < routeNodes.length - 1
              ? `<i aria-hidden="true"><b>${signal}</b>&rarr;</i>`
              : "";
            return `<span>${escapeHtml(node)}</span>${edge}`;
          })
          .join("");
        return `<section class="channel-card" data-channel-track="${track.id}" tabindex="-1" style="--track-color:${track.color}">
          <div class="channel-heading">
            <div class="channel-name"><i></i>${escapeHtml(track.name)}</div>
            <div class="channel-actions">
              <button class="mute-button ${track.muted ? "is-muted" : ""}" type="button" data-mute-track="${track.id}" data-muted="${track.muted}">${track.muted ? "MUTED" : "MUTE"}</button>
              <button class="delete-channel-button" type="button" data-delete-track="${track.id}" data-track-name="${escapeHtml(track.name)}" aria-label="${escapeHtml(`Delete ${track.name} channel`)}" ${state.channelMutationPending ? "disabled" : ""}>Delete</button>
            </div>
          </div>
          <label class="volume-control">LEVEL
            <input type="range" min="0" max="1.5" step="0.01" value="${track.volume}" data-volume-track="${track.id}" aria-label="${escapeHtml(track.name)} volume">
            <output>${Math.round(track.volume * 100)}%</output>
          </label>
          <div class="sound-tool instrument-tool">
            <div class="tool-heading"><div><span>Instrument</span><strong>${escapeHtml(track.instrument.engine)}</strong></div><code>#${track.instrument.id}</code></div>
            <div class="oscillator-stack">
              ${track.instrument.oscillators.map((oscillator, index) => renderOscillator(track, oscillator, index)).join("")}
            </div>
            <div class="tool-controls instrument-envelope-controls">
              ${soundRange(track, "instrument", track.instrument.id, "instrument", "attack", track.instrument.parameters.attack, 0.001, 2, "s")}
              ${soundRange(track, "instrument", track.instrument.id, "instrument", "release", track.instrument.parameters.release, 0.02, 5, "s")}
              ${soundRange(track, "instrument", track.instrument.id, "instrument", "tone", track.instrument.parameters.tone, 0, 1, "%")}
            </div>
          </div>
          <div class="sound-tool effects-tool">
            <div class="tool-heading"><div><span>Effect chain</span><strong>Processed in this order</strong></div></div>
            <div class="routing-chain" aria-label="${escapeHtml(track.name)} typed sound routing">${route}</div>
            <div class="effect-stack">${orderedEffects.map((effect, index) => renderEffect(track, effect, index, orderedEffects.length)).join("")}</div>
            <div class="effects-list">${regionalEffects || '<span class="effect-pill">No regional effects</span>'}</div>
          </div>
          <div class="sound-tool modulators-tool">
            <div class="tool-heading"><div><span>Modulators</span><strong>Time-varying control signals</strong></div></div>
            ${track.modulators.map((modulator) => renderModulator(track, modulator)).join("")}
          </div>
          <div class="sound-tool clips-tool">
            <div class="tool-heading"><div><span>MIDI Clips</span><strong>Timed notes, pitches, and velocities</strong></div></div>
            ${track.clips.map((clip) => renderClipEvents(track, clip)).join("") || '<span class="effect-pill">No MIDI clips</span>'}
          </div>
        </section>`;
      })
      .join("");
    restoreAdvancedUiState(uiState);

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
    elements.channelList.querySelectorAll("[data-delete-track]").forEach((button) => {
      button.addEventListener("click", () => {
        if (!window.confirm(`Delete the ${button.dataset.trackName} channel and all of its sound tools?`)) return;
        void changeChannel("delete", { track_id: button.dataset.deleteTrack });
      });
    });
    elements.channelList.querySelectorAll("[data-sound-tool]").forEach((control) => {
      validateSoundToolControl(control);
      if (control.matches('input[type="range"]')) {
        control.addEventListener("input", () => {
          validateSoundToolControl(control);
          const output = control.nextElementSibling;
          if (output?.matches("output")) output.value = formatSoundValue(control.value, control.dataset.unit);
        });
      }
      control.addEventListener("change", () => {
        validateSoundToolControl(control);
        void changeSoundTool(control, control.value);
      });
    });
    elements.channelList.querySelectorAll("[data-sound-value]").forEach((button) => {
      button.addEventListener("click", () => {
        void changeSoundTool(button, button.dataset.soundValue);
      });
    });
  }

  function captureAdvancedUiState() {
    const clips = new Map();
    for (const editor of elements.channelList.querySelectorAll("[data-clip-key]")) {
      const events = editor.querySelector(".clip-event-list");
      clips.set(editor.dataset.clipKey, {
        open: editor.open,
        scrollTop: events?.scrollTop ?? 0,
        scrollLeft: events?.scrollLeft ?? 0,
      });
    }
    return { drawerScrollTop: elements.advancedDrawer.scrollTop, clips };
  }

  function restoreAdvancedUiState(uiState) {
    elements.advancedDrawer.scrollTop = uiState.drawerScrollTop;
    for (const editor of elements.channelList.querySelectorAll("[data-clip-key]")) {
      const clipState = uiState.clips.get(editor.dataset.clipKey);
      if (!clipState) continue;
      editor.open = clipState.open;
      const events = editor.querySelector(".clip-event-list");
      if (!events) continue;
      events.scrollTop = clipState.scrollTop;
      events.scrollLeft = clipState.scrollLeft;
    }
  }

  function soundRange(
    track,
    tool,
    toolId,
    ownerName,
    parameter,
    value,
    minimum,
    maximum,
    unit,
    clipId = "",
    label = parameter,
  ) {
    const key = `${track.id}-${tool}-${toolId}-${parameter}`;
    const clipAttribute = clipId === "" ? "" : ` data-clip-id="${clipId}"`;
    const owner = tool === "instrument" ? "instrument" : `${ownerName} ${tool}`;
    const accessibleName = `${track.name} ${owner} #${toolId} ${parameter}`;
    return `<label class="tool-control">${escapeHtml(label)}
      <span class="range-with-output"><input type="range" min="${minimum}" max="${maximum}" step="any" value="${value}" data-sound-tool="${tool}" data-track-id="${track.id}" data-tool-id="${toolId}" data-parameter="${parameter}" data-unit="${unit}" data-control-key="${key}"${clipAttribute} aria-label="${escapeHtml(accessibleName)}"><output>${formatSoundValue(value, unit)}</output></span>
    </label>`;
  }

  function renderOscillator(track, oscillator, index) {
    const number = index + 1;
    const prefix = `oscillator${number}`;
    const waveformParameter = index === 0 ? "waveform" : `${prefix}.waveform`;
    return `<div class="oscillator-card">
      <strong>Oscillator ${number}</strong>
      <div class="tool-controls">
        <label class="tool-control">Waveform
          <select data-sound-tool="instrument" data-track-id="${track.id}" data-tool-id="${track.instrument.id}" data-parameter="${waveformParameter}" data-control-key="${track.id}-instrument-${track.instrument.id}-${waveformParameter}" aria-label="${escapeHtml(`${track.name} instrument #${track.instrument.id} oscillator ${number} waveform`)}">
            ${selectOptions(["sine", "triangle", "sawtooth", "square"], oscillator.waveform)}
          </select>
        </label>
        ${soundRange(track, "instrument", track.instrument.id, "instrument", `${prefix}.tuning`, oscillator.tuning, -24, 24, "st", "", "Tuning")}
        ${soundRange(track, "instrument", track.instrument.id, "instrument", `${prefix}.level`, oscillator.level, 0, 1, "%", "", "Level")}
      </div>
    </div>`;
  }

  function soundToggle(track, tool, toolId, name, enabled) {
    const action = enabled ? "Disable" : "Enable";
    const accessibleName = `${action} ${track.name} ${name} ${tool} #${toolId}`;
    return `<button type="button" aria-label="${escapeHtml(accessibleName)}" aria-pressed="${String(enabled)}" data-sound-tool="${tool}" data-track-id="${track.id}" data-tool-id="${toolId}" data-parameter="enabled" data-sound-value="${String(!enabled)}" data-control-key="${track.id}-${tool}-${toolId}-enabled">${enabled ? "On" : "Off"}</button>`;
  }

  function renderEffect(track, effect, index, effectCount) {
    const filterControls = Number.isFinite(effect.parameters.cutoff)
      ? [
          soundRange(
            track,
            "effect",
            effect.id,
            effect.name,
            "cutoff",
            effect.parameters.cutoff,
            80,
            16000,
            "Hz",
            "",
            "Cutoff",
          ),
          soundRange(
            track,
            "effect",
            effect.id,
            effect.name,
            "resonance",
            effect.parameters.resonance,
            0.1,
            20,
            "Q",
            "",
            "Resonance",
          ),
        ].join("")
      : "";
    return `<div class="effect-card ${effect.enabled ? "" : "is-disabled"}">
      <div class="effect-card-heading"><span class="effect-pill"><strong>${escapeHtml(effect.name)}</strong> <b>${formatSoundValue(effect.parameters.mix, "%")}</b></span><code>#${effect.id}</code></div>
      ${soundRange(track, "effect", effect.id, effect.name, "mix", effect.parameters.mix, 0, 1, "%")}
      ${filterControls}
      <div class="tool-actions">
        ${soundToggle(track, "effect", effect.id, effect.name, effect.enabled)}
        <button type="button" aria-label="${escapeHtml(`Move ${track.name} ${effect.name} effect #${effect.id} earlier`)}" ${index === 0 ? "disabled" : ""} data-sound-tool="routing" data-track-id="${track.id}" data-tool-id="${effect.id}" data-parameter="position" data-sound-value="${Math.max(0, index - 1)}" data-control-key="${track.id}-routing-${effect.id}-up">&uarr;</button>
        <button type="button" aria-label="${escapeHtml(`Move ${track.name} ${effect.name} effect #${effect.id} later`)}" ${index === effectCount - 1 ? "disabled" : ""} data-sound-tool="routing" data-track-id="${track.id}" data-tool-id="${effect.id}" data-parameter="position" data-sound-value="${Math.min(effectCount - 1, index + 1)}" data-control-key="${track.id}-routing-${effect.id}-down">&darr;</button>
      </div>
    </div>`;
  }

  function renderModulator(track, modulator) {
    const targets = track.modulationTargets.map((target) => [target.id, target.label]);
    return `<div class="modulator-card ${modulator.enabled ? "" : "is-disabled"}">
      <div class="effect-card-heading"><strong>${escapeHtml(modulator.name)}</strong><code>#${modulator.id}</code></div>
      ${modulator.enabled && modulator.trigger === "midi" ? '<div class="modulator-route"><b>MIDI Clips</b><i aria-hidden="true">MIDI &rarr;</i><b>Modulator</b></div>' : ""}
      <div class="tool-controls">
        <label class="tool-control">Shape
          <select data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="shape" data-control-key="${track.id}-modulator-${modulator.id}-shape" aria-label="${escapeHtml(`${track.name} ${modulator.name} modulator #${modulator.id} shape`)}">${selectOptions(["sine", "triangle", "square", "random", "envelope"], modulator.shape)}</select>
        </label>
        <label class="tool-control">Target
          <select data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="target" data-control-key="${track.id}-modulator-${modulator.id}-target" aria-label="${escapeHtml(`${track.name} ${modulator.name} modulator #${modulator.id} target`)}">${targets.map(([value, label]) => `<option value="${value}" ${value === modulator.target ? "selected" : ""}>${escapeHtml(label)}</option>`).join("")}</select>
        </label>
        <label class="tool-control">Rate mode
          <select data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="rateMode" data-control-key="${track.id}-modulator-${modulator.id}-rateMode" aria-label="${escapeHtml(`${track.name} ${modulator.name} modulator #${modulator.id} rate mode`)}">${selectOptions(["hz", "tempo"], modulator.rateMode)}</select>
        </label>
        <label class="tool-control">Trigger
          <select data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="trigger" data-control-key="${track.id}-modulator-${modulator.id}-trigger" aria-label="${escapeHtml(`${track.name} ${modulator.name} modulator #${modulator.id} trigger`)}">${selectOptions(["free", "midi"], modulator.trigger)}</select>
        </label>
        ${soundRange(track, "modulator", modulator.id, modulator.name, "rate", modulator.parameters.rate, 0.01, 20, modulator.rateMode === "tempo" ? "x/beat" : "Hz")}
        ${soundRange(track, "modulator", modulator.id, modulator.name, "depth", modulator.parameters.depth, 0, 1, "%")}
      </div>
      <div class="tool-actions">${soundToggle(track, "modulator", modulator.id, modulator.name, modulator.enabled)}</div>
    </div>`;
  }

  function renderClipEvents(track, clip) {
    const rows = clip.events
      .map((event) => {
        const key = `${track.id}-clip-${clip.id}-event-${event.id}`;
        const accessibleName = `${track.name} ${clip.label} clip #${clip.id} ${event.type} event #${event.id}`;
        return `<div class="clip-event" data-event-id="${event.id}">
          <strong>${escapeHtml(event.type)}</strong>
          <label>Beat<input type="number" min="0" max="${clip.loopBeats}" step="any" value="${event.time}" data-maximum-exclusive="${clip.loopBeats}" data-sound-tool="event" data-track-id="${track.id}" data-tool-id="${event.id}" data-clip-id="${clip.id}" data-parameter="time" data-control-key="${key}-time" aria-label="${escapeHtml(`${accessibleName} beat`)}"></label>
          <label>Length<input type="number" min="0.0625" max="${clip.loopBeats}" step="any" value="${event.duration}" data-sound-tool="event" data-track-id="${track.id}" data-tool-id="${event.id}" data-clip-id="${clip.id}" data-parameter="duration" data-control-key="${key}-duration" aria-label="${escapeHtml(`${accessibleName} length`)}"></label>
          <label>Pitch<input type="number" min="0" max="127" step="1" value="${event.pitch}" data-sound-tool="event" data-track-id="${track.id}" data-tool-id="${event.id}" data-clip-id="${clip.id}" data-parameter="pitch" data-control-key="${key}-pitch" aria-label="${escapeHtml(`${accessibleName} pitch`)}"></label>
          <label>Velocity<input type="number" min="0.01" max="1" step="any" value="${event.velocity}" data-sound-tool="event" data-track-id="${track.id}" data-tool-id="${event.id}" data-clip-id="${clip.id}" data-parameter="velocity" data-control-key="${key}-velocity" aria-label="${escapeHtml(`${accessibleName} velocity`)}"></label>
        </div>`;
      })
      .join("");
    return `<details class="clip-editor" data-clip-key="${track.id}-${clip.id}" open><summary><span>${escapeHtml(clip.label)}</span><b>${clip.events.length} events &middot; ${clip.loopBeats} beat loop</b></summary><div class="clip-event-list">${rows}</div></details>`;
  }

  function selectOptions(values, selected) {
    return values
      .map((value) => `<option value="${value}" ${value === selected ? "selected" : ""}>${escapeHtml(value)}</option>`)
      .join("");
  }

  function formatSoundValue(value, unit) {
    const number = Number(value);
    if (unit === "%") return `${Number((number * 100).toFixed(4))}%`;
    if (unit === "Hz") return `${Number(number.toFixed(6))} Hz`;
    if (unit === "s") return `${Number(number.toFixed(6))} s`;
    return String(value);
  }

  function validateSoundToolControl(control) {
    const maximum = Number(control.dataset.maximumExclusive);
    if (!Number.isFinite(maximum)) return;
    const valid = Number.isFinite(control.valueAsNumber) && control.valueAsNumber < maximum;
    control.setCustomValidity(valid ? "" : `Enter a value below ${maximum}`);
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
    if (action.type === "timed") {
      const duration = edit.end - edit.start;
      collectRegionalEffects(action.action, role, {
        start: edit.start + duration * action.start,
        end: edit.start + duration * action.end,
      }, effects);
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

  async function replaceProject(operation, options = {}) {
    const preservePosition = options.preservePosition !== false;
    const resumePlayback = options.resumePlayback !== false && audio.isActive;
    audio.stop(preservePosition);
    let projectReplaced = false;
    try {
      const result = await operation();
      projectReplaced = true;
      return result;
    } finally {
      const startedDuringReplacement = audio.isActive;
      if (projectReplaced && startedDuringReplacement) audio.stop(preservePosition);
      if ((resumePlayback || startedDuringReplacement) && !audio.isActive) await audio.start();
    }
  }

  function enqueueProjectMutation(operation) {
    const queuedMutation = projectMutationQueue.then(operation, operation);
    projectMutationQueue = queuedMutation.catch(() => {});
    return queuedMutation;
  }

  function changeMix(trackId, values, focusControl) {
    return enqueueProjectMutation(() => applyMixChange(trackId, values, focusControl));
  }

  function setChannelMutationPending(pending) {
    state.channelMutationPending = pending;
    elements.addChannel.disabled = pending;
    elements.channelList.querySelectorAll("[data-delete-track]").forEach((button) => {
      button.disabled = pending;
    });
  }

  function channelMutationRecord(project, clientOperationId, action, values) {
    const record = project.channelOperations?.find(
      (operation) => operation.operationId === clientOperationId,
    );
    if (!record || record.action !== action) return null;
    if (action === "add" && record.role !== values.role) return null;
    if (action === "delete" && String(record.trackId) !== String(values.track_id)) return null;
    return record;
  }

  function focusChannelMutation(operation) {
    if (operation.action === "add") {
      elements.channelList
        .querySelector(`[data-channel-track="${operation.trackId}"]`)
        ?.focus({ preventScroll: true });
    } else {
      elements.addChannel.focus({ preventScroll: true });
    }
  }

  function changeChannel(action, values) {
    if (state.channelMutationPending) return Promise.resolve();
    const clientOperationId = operationId();
    setChannelMutationPending(true);
    return enqueueProjectMutation(async () => {
      let reconciled = true;
      let confirmedOperation = null;
      try {
        await replaceProject(async () => {
          const project = await api("/api/channels", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({ operation_id: clientOperationId, action, ...values }),
          });
          confirmedOperation = channelMutationRecord(
            project,
            clientOperationId,
            action,
            values,
          );
          if (!confirmedOperation) throw new Error("The channel response did not identify this mutation.");
          state.project = project;
          renderProject();
        });
        showToast(action === "add" ? "Channel added" : "Channel deleted");
      } catch (error) {
        if (isRetryableApiError(error)) {
          try {
            await replaceProject(async () => {
              state.project = await api("/api/project", {}, RECONCILED_REQUEST_TIMEOUT_MS);
              renderProject();
            });
            confirmedOperation = channelMutationRecord(
              state.project,
              clientOperationId,
              action,
              values,
            );
            if (confirmedOperation) {
              showToast(action === "add" ? "Channel added" : "Channel deleted");
            } else {
              showError(error, action === "add" ? "adding a channel" : "deleting a channel");
            }
          } catch (refreshError) {
            reconciled = false;
            showError(
              new Error(
                `The channel result could not be confirmed. Reload before trying again. ${errorMessage(refreshError)}`,
              ),
              action === "add" ? "adding a channel" : "deleting a channel",
            );
          }
        } else {
          showError(error, action === "add" ? "adding a channel" : "deleting a channel");
        }
      } finally {
        if (reconciled) {
          setChannelMutationPending(false);
          if (confirmedOperation) focusChannelMutation(confirmedOperation);
        }
      }
    });
  }

  function changeSoundTool(control, value) {
    const request = {
      track_id: control.dataset.trackId,
      tool: control.dataset.soundTool,
      tool_id: control.dataset.toolId,
      parameter: control.dataset.parameter,
      value: String(value),
    };
    if (control.dataset.clipId) request.clip_id = control.dataset.clipId;
    if (!control.checkValidity()) {
      renderProject();
      restoreSoundToolFocus(request, control.dataset.controlKey);
      showToast("Enter a value within the supported range", true);
      return Promise.resolve();
    }
    return enqueueProjectMutation(() => applySoundToolChange(request, control.dataset.controlKey));
  }

  function restoreSoundToolFocus(request, focusKey) {
    const controls = [...elements.channelList.querySelectorAll("[data-control-key]")];
    const exactControl = controls.find((candidate) => candidate.dataset.controlKey === focusKey);
    const sameToolControls = controls.filter(
      (candidate) =>
        candidate.dataset.trackId === request.track_id &&
        candidate.dataset.toolId === request.tool_id &&
        !candidate.disabled,
    );
    const fallbackControl =
      sameToolControls.find((candidate) => candidate.dataset.soundTool === request.tool) ?? sameToolControls[0];
    const focusControl = exactControl && !exactControl.disabled ? exactControl : fallbackControl;
    if (!focusControl) return;
    focusControl.focus({ preventScroll: true });
    revealEventControl(focusControl);
  }

  function revealEventControl(control) {
    const eventList = control.closest(".clip-event-list");
    if (!eventList) return;
    const listRect = eventList.getBoundingClientRect();
    const controlRect = control.getBoundingClientRect();
    if (controlRect.top < listRect.top) {
      eventList.scrollTop -= listRect.top - controlRect.top;
    } else if (controlRect.bottom > listRect.bottom) {
      eventList.scrollTop += controlRect.bottom - listRect.bottom;
    }
    if (controlRect.left < listRect.left) {
      eventList.scrollLeft -= listRect.left - controlRect.left;
    } else if (controlRect.right > listRect.right) {
      eventList.scrollLeft += controlRect.right - listRect.right;
    }
  }

  async function applySoundToolChange(request, focusKey) {
    try {
      await replaceProject(async () => {
        state.project = await api("/api/sound-tools", {
          method: "POST",
          headers: { "Content-Type": "application/x-www-form-urlencoded" },
          body: new URLSearchParams(request),
        });
        renderProject();
        restoreSoundToolFocus(request, focusKey);
      });
      showToast("Sound tool updated");
    } catch (error) {
      renderProject();
      restoreSoundToolFocus(request, focusKey);
      showError(error, "updating a sound tool");
    }
  }

  async function applyMixChange(trackId, values, focusControl) {
    try {
      await replaceProject(async () => {
        state.project = await api("/api/mix", {
          method: "POST",
          headers: { "Content-Type": "application/x-www-form-urlencoded" },
          body: new URLSearchParams({ track_id: trackId, ...values }),
        });
        renderProject();
        const selector =
          focusControl === "volume" ? `[data-volume-track="${trackId}"]` : `[data-mute-track="${trackId}"]`;
        elements.channelList.querySelector(selector)?.focus({ preventScroll: true });
      });
    } catch (error) {
      showError(error, "updating the mixer");
    }
  }

  function editAppliesToTrack(edit, track) {
    return actionAppliesToTrack(edit.action, track);
  }

  function actionAppliesToTrack(action, track) {
    if (action.type === "compound") return action.actions.some((child) => actionAppliesToTrack(child, track));
    if (action.type === "timed") return actionAppliesToTrack(action.action, track);
    if (action.type === "automation") return action.trackId === track.id;
    return action.target === "all" || action.target === track.role;
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
    state.dragAnchor = timelineTimeFromPointer(event);
    state.selectionStart = Math.min(state.dragAnchor, state.project.duration - 0.25);
    state.selectionEnd = state.selectionStart + 0.25;
    elements.trackRows.setPointerCapture(event.pointerId);
    renderSelection();
  }

  function moveSelection(event) {
    if (event.pointerId !== state.dragPointer) return;
    const current = timelineTimeFromPointer(event);
    if (current === state.dragAnchor) {
      state.selectionStart = Math.min(state.dragAnchor, state.project.duration - 0.25);
      state.selectionEnd = state.selectionStart + 0.25;
      renderSelection();
      return;
    }
    state.selectionStart = Math.min(state.dragAnchor, current);
    state.selectionEnd = Math.max(state.dragAnchor, current);
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

  function showEditProgress(job) {
    const elapsed = Math.max(0, Number(job.elapsedSeconds) || 0);
    const timeout = Math.max(1, Number(job.timeoutSeconds) || 20 * 60);
    const detail = job.detail || "Gemini is working on the edit";
    const appliedSteps = Math.max(0, Number(job.appliedSteps) || 0);
    let nextActivityPercent = 5;
    if (job.status === "completed") {
      nextActivityPercent = 100;
    } else if (job.phase === "syncing") {
      nextActivityPercent = state.editProgressPercent;
    } else if (job.phase === "finalizing") {
      nextActivityPercent = 94;
    } else if (job.phase === "applying") {
      nextActivityPercent = 88;
    } else if (appliedSteps > 0) {
      nextActivityPercent = 90 - 70 / (appliedSteps + 1);
    } else if (job.phase === "planning") {
      nextActivityPercent = 14;
    }
    state.editProgressPercent = Math.max(state.editProgressPercent, nextActivityPercent);
    elements.editProgress.hidden = false;
    if (elements.editProgressLabel.textContent !== detail) elements.editProgressLabel.textContent = detail;
    elements.editProgressTime.textContent = `${formatTime(elapsed, false)} / ${formatTime(timeout, false)}`;
    elements.editProgressFill.style.width = `${state.editProgressPercent}%`;
    elements.editProgressTrack.setAttribute(
      "aria-valuetext",
      appliedSteps > 0 ? `${appliedSteps} edit ${appliedSteps === 1 ? "step" : "steps"} applied. ${detail}` : detail,
    );
    elements.editProgressTrack.removeAttribute("aria-valuenow");
    elements.savedState.textContent = `${detail} - ${formatTime(elapsed, false)} elapsed`;
    elements.composeButton.querySelector("span").textContent =
      job.phase === "syncing"
        ? "Refreshing project..."
        : job.phase === "applying"
          ? "Applying change..."
          : "Gemini is working...";
  }

  function hideEditProgress() {
    elements.editProgress.hidden = true;
    state.editProgressPercent = 0;
    elements.editProgressFill.style.width = "0%";
  }

  function wait(milliseconds) {
    return new Promise((resolve) => window.setTimeout(resolve, milliseconds));
  }

  function operationId() {
    if (typeof window.crypto.randomUUID === "function") return window.crypto.randomUUID();
    const bytes = window.crypto.getRandomValues(new Uint8Array(16));
    return `client-${Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
  }

  function readPendingEdit() {
    try {
      const serialized = window.localStorage.getItem(PENDING_EDIT_STORAGE_KEY);
      if (!serialized) return null;
      const pending = JSON.parse(serialized);
      const validOperationId =
        typeof pending.operationId === "string" && /^[A-Za-z0-9_-]{1,128}$/.test(pending.operationId);
      const validRequest =
        typeof pending.prompt === "string" &&
        pending.prompt.length > 0 &&
        typeof pending.submittedText === "string" &&
        Number.isFinite(pending.start) &&
        Number.isFinite(pending.end) &&
        pending.start < pending.end;
      const validJob =
        pending.acceptedJob === null ||
        (typeof pending.acceptedJob === "object" &&
          typeof pending.acceptedJob.id === "string" &&
          pending.acceptedJob.operationId === pending.operationId);
      if (validOperationId && validRequest && validJob) return pending;
    } catch (_error) {
      // Invalid or unavailable storage must not prevent the studio from loading.
    }
    try {
      window.localStorage.removeItem(PENDING_EDIT_STORAGE_KEY);
    } catch (_error) {
      // Ignore unavailable storage.
    }
    return null;
  }

  function persistPendingEdit(pending) {
    try {
      window.localStorage.setItem(PENDING_EDIT_STORAGE_KEY, JSON.stringify(pending));
    } catch (error) {
      reportClientIssue("warning", error, "persisting an active edit");
    }
  }

  function clearPendingEdit(clientOperationId) {
    try {
      const serialized = window.localStorage.getItem(PENDING_EDIT_STORAGE_KEY);
      if (!serialized) return;
      const pending = JSON.parse(serialized);
      if (pending.operationId === clientOperationId) {
        window.localStorage.removeItem(PENDING_EDIT_STORAGE_KEY);
      }
    } catch (_error) {
      // Ignore unavailable storage.
    }
  }

  function requestTimeout(deadline) {
    return Math.min(RECONCILED_REQUEST_TIMEOUT_MS, Math.max(0, Math.floor(deadline - performance.now())));
  }

  async function acceptEdit(clientOperationId, requestBody, onFirstAttempt) {
    const deadline = performance.now() + EDIT_ACCEPTANCE_TIMEOUT_MS;
    let failures = 0;
    let firstAttempt = true;
    for (;;) {
      const timeout = requestTimeout(deadline);
      if (timeout === 0) {
        return {
          status: "unavailable",
          operationId: clientOperationId,
          error: new Error("The studio did not confirm whether it accepted the edit."),
        };
      }
      try {
        const job = await enqueueProjectMutation(async () => {
          if (firstAttempt) {
            firstAttempt = false;
            onFirstAttempt();
          }
          return api(
            "/api/edits",
            {
              method: "POST",
              headers: { "Content-Type": "application/x-www-form-urlencoded" },
              body: requestBody,
            },
            timeout,
          );
        });
        if (job.operationId !== clientOperationId) {
          return {
            status: "unavailable",
            operationId: clientOperationId,
            error: new Error("The studio returned a different edit operation."),
          };
        }
        return job;
      } catch (error) {
        if (error instanceof ApiError && !error.retryable) {
          if (failures === 0) throw error;
          return { status: "unavailable", operationId: clientOperationId, error };
        }
        if (performance.now() >= deadline) {
          return { status: "unavailable", operationId: clientOperationId, error };
        }
        failures += 1;
        showEditProgress({
          phase: "queued",
          detail: "Connection interrupted; confirming the edit was accepted",
          elapsedSeconds: Math.floor((EDIT_ACCEPTANCE_TIMEOUT_MS - (deadline - performance.now())) / 1000),
          timeoutSeconds: EDIT_ACCEPTANCE_TIMEOUT_MS / 1000,
        });
        await wait(clamp(250 * 2 ** (failures - 1), 250, 5000));
      }
    }
  }

  async function pollAcceptedEdit(initialJob) {
    let job = initialJob;
    let consecutivePollFailures = 0;
    const remainingSeconds = Math.max(
      0,
      (Number(initialJob.timeoutSeconds) || 20 * 60) - (Number(initialJob.elapsedSeconds) || 0),
    );
    const visibilityDeadline = performance.now() + remainingSeconds * 1000 + 30_000;
    for (;;) {
      showEditProgress(job);
      const publishedVersion = Number(job.projectVersion);
      if (Number.isFinite(publishedVersion) && publishedVersion > (state.project?.version ?? 0)) {
        try {
          await refreshAuthoritativeProject(
            `Showing Gemini step ${Number(job.appliedSteps) || 1}`,
            Math.min(visibilityDeadline, performance.now() + RECONCILED_REQUEST_TIMEOUT_MS),
          );
          showEditProgress(job);
        } catch (_error) {
          job = { ...job, detail: "Gemini applied a step; retrying the project refresh" };
          showEditProgress(job);
        }
      }
      if (job.status === "completed") return job;
      if (job.status === "failed") return job;
      if (job.status !== "queued" && job.status !== "running") {
        return {
          ...job,
          status: "unavailable",
          error: new Error("The studio returned an unknown edit status."),
        };
      }
      const serverPollAfter = clamp(Number(job.pollAfterMs) || 1000, 20, 5000);
      const pollAfter = clamp(serverPollAfter * 2 ** consecutivePollFailures, 20, 5000);
      await wait(pollAfter);
      const timeout = requestTimeout(visibilityDeadline);
      if (timeout === 0) {
        return {
          ...job,
          status: "unavailable",
          error: new Error("The edit status polling deadline expired."),
        };
      }
      try {
        const nextJob = await api(`/api/edits/${encodeURIComponent(job.id)}`, {}, timeout);
        if (nextJob.operationId !== initialJob.operationId) {
          return {
            ...job,
            status: "unavailable",
            error: new Error("The edit job identity changed."),
          };
        }
        job = nextJob;
        consecutivePollFailures = 0;
      } catch (error) {
        if (!isRetryableApiError(error) || performance.now() >= visibilityDeadline) {
          return { ...job, status: "unavailable", error };
        }
        consecutivePollFailures += 1;
        job = {
          ...job,
          detail: "Connection interrupted; still waiting for the accepted edit",
          elapsedSeconds: (Number(job.elapsedSeconds) || 0) + Math.ceil(pollAfter / 1000),
        };
      }
    }
  }

  async function refreshAuthoritativeProject(detail, deadline = performance.now() + 30_000) {
    let failures = 0;
    for (;;) {
      showEditProgress({
        phase: "syncing",
        detail: failures === 0 ? detail : "Connection interrupted; retrying the project refresh",
        elapsedSeconds: 0,
        timeoutSeconds: 30,
      });
      if (requestTimeout(deadline) === 0) throw new Error("The project refresh deadline expired.");
      try {
        return await enqueueProjectMutation(() =>
          replaceProject(async () => {
            const timeout = requestTimeout(deadline);
            if (timeout === 0) throw new Error("The project refresh deadline expired.");
            state.project = await api("/api/project", {}, timeout);
            renderProject();
            return state.project;
          }),
        );
      } catch (error) {
        if (!isRetryableApiError(error) || performance.now() >= deadline) throw error;
        const retryAfter = clamp(250 * 2 ** failures, 250, 5000);
        failures += 1;
        await wait(retryAfter);
      }
    }
  }

  function persistedOperationOutcome(operation) {
    const completed = operation.status === "completed";
    return {
      id: "recovered",
      operationId: operation.operationId,
      status: completed ? "completed" : "failed",
      phase: completed ? "completed" : "failed",
      message: completed ? operation.message : undefined,
      error: completed ? undefined : "Gemini stopped before completing the edit.",
      errorStatus: completed ? undefined : 500,
      appliedSteps: Number(operation.appliedSteps) || 0,
      projectVersion: Number(operation.projectVersion) || null,
    };
  }

  async function reconcileUnavailableOperation(operationId) {
    const deadline = performance.now() + 2_000;
    for (;;) {
      try {
        const outcome = await api(
          `/api/edit-operations/${encodeURIComponent(operationId)}`,
          {},
          requestTimeout(deadline),
        );
        const publishedVersion = Number(outcome.projectVersion);
        if (Number.isFinite(publishedVersion) && publishedVersion > (state.project?.version ?? 0)) {
          await refreshAuthoritativeProject("Edit status recovered; refreshing the project", deadline);
        }
        return outcome;
      } catch (error) {
        if (!(error instanceof ApiError && error.status === 404) && !isRetryableApiError(error)) {
          throw error;
        }
      }
      let project;
      try {
        project = await refreshAuthoritativeProject(
          "Edit status unavailable; checking the current project",
          deadline,
        );
      } catch (error) {
        if (performance.now() >= deadline) return null;
        throw error;
      }
      const operation = project.editOperations?.find(
        (candidate) => candidate.operationId === operationId,
      );
      if (operation) return persistedOperationOutcome(operation);
      const committedEdit = project.edits.find((edit) => edit.operationId === operationId);
      if (committedEdit) {
        return persistedOperationOutcome({
          operationId,
          status: "completed",
          appliedSteps: 1,
          projectVersion: project.version,
          message: committedEdit.summary,
        });
      }
      if (performance.now() >= deadline) return null;
      await wait(100);
    }
  }

  function appliedEditSteps(outcome) {
    const steps = Number(outcome.appliedSteps);
    return Number.isFinite(steps) ? Math.max(0, Math.floor(steps)) : 0;
  }

  function partialEditError(outcome, refreshError = null) {
    const steps = appliedEditSteps(outcome);
    const rawReason =
      outcome.status === "unavailable"
        ? "The edit status was lost."
        : `${outcome.error || "Gemini could not complete the edit."}`.trim();
    const reason = /[.!?]$/.test(rawReason) ? rawReason : `${rawReason}.`;
    const savedChanges = steps === 1 ? "1 partial change was saved" : `${steps} partial changes were saved`;
    const refreshWarning = refreshError
      ? ` Reload to see the latest saved state. ${errorMessage(refreshError)}`
      : "";
    return new Error(`${reason} ${savedChanges}; review the project before retrying.${refreshWarning}`);
  }

  async function resolveEditOutcome(outcome) {
    if (outcome.status === "completed") {
      return { kind: "completed", message: outcome.message, refresh: true };
    }

    const hasPublishedChanges = appliedEditSteps(outcome) > 0;
    if (outcome.status === "unavailable") {
      if (hasPublishedChanges) return { kind: "partial", error: partialEditError(outcome) };
      return {
        kind: "failed",
        error: new Error("The edit status was lost. The current project was refreshed; review it before retrying."),
      };
    }

    if (outcome.status === "failed") {
      if (hasPublishedChanges) {
        let refreshError = null;
        try {
          await refreshAuthoritativeProject("Gemini stopped; refreshing its partial changes");
        } catch (error) {
          refreshError = error;
        }
        return { kind: "partial", error: partialEditError(outcome, refreshError) };
      }
      if (Number(outcome.errorStatus) === 409) {
        await refreshAuthoritativeProject("The project changed; loading its current version");
      }
      return {
        kind: "failed",
        error: new Error(outcome.error || "Gemini could not complete the edit."),
      };
    }

    return { kind: "failed", error: new Error("The studio returned an unknown edit status.") };
  }

  function showPendingEdit(detail) {
    state.promptPending = true;
    elements.composeButton.disabled = true;
    elements.composeButton.querySelector("span").textContent = "Starting Gemini...";
    elements.savedState.textContent = "Waiting for Gemini";
    showEditProgress({
      phase: "queued",
      detail,
      elapsedSeconds: 0,
      timeoutSeconds: 20 * 60,
    });
  }

  async function runPendingEdit(pending, capturePlayback) {
    const {
      operationId: clientOperationId,
      prompt,
      start: selectionStart,
      end: selectionEnd,
      submittedText,
    } = pending;
    let clearSubmittedPrompt = false;
    let restorePlayback = false;
    let playbackStateCaptured = false;
    showPendingEdit(pending.acceptedJob ? "Reconnecting to the active AI edit" : "Starting the AI edit");
    try {
      let accepted = pending.acceptedJob;
      if (!accepted) {
        accepted = await acceptEdit(
          clientOperationId,
          new URLSearchParams({
            operation_id: clientOperationId,
            prompt,
            start: String(selectionStart),
            end: String(selectionEnd),
          }),
          () => {
            if (!capturePlayback) return;
            restorePlayback = audio.isActive;
            playbackStateCaptured = true;
            audio.stop(true);
          },
        );
        if (accepted.status !== "unavailable") {
          pending.acceptedJob = accepted;
          persistPendingEdit(pending);
        }
      }
      let outcome = accepted.status === "unavailable" ? accepted : await pollAcceptedEdit(accepted);
      if (outcome.status === "unavailable") {
        const recovered = await reconcileUnavailableOperation(clientOperationId);
        if (recovered) {
          if (recovered.status === "queued" || recovered.status === "running") {
            pending.acceptedJob = recovered;
            persistPendingEdit(pending);
            outcome = await pollAcceptedEdit(recovered);
          } else {
            outcome = recovered;
          }
        }
      }
      const result = await resolveEditOutcome(outcome);
      clearSubmittedPrompt = result.kind === "completed" || result.kind === "partial";
      if (result.kind === "completed" && result.refresh) {
        try {
          await refreshAuthoritativeProject("Edit completed; refreshing the project");
        } catch (error) {
          throw new CommittedEditSyncError(error);
        }
      }
      if (clearSubmittedPrompt && elements.promptInput.value === submittedText) {
        elements.promptInput.value = "";
      }
      if (result.kind !== "completed") throw result.error;
      showToast(result.message);
    } catch (error) {
      if (clearSubmittedPrompt && elements.promptInput.value === submittedText) {
        elements.promptInput.value = "";
      }
      showError(error, "applying a prompted edit");
      elements.savedState.textContent = state.project ? `Version ${state.project.version}` : "Offline";
    } finally {
      clearPendingEdit(clientOperationId);
      state.promptPending = false;
      hideEditProgress();
      elements.composeButton.disabled = false;
      elements.composeButton.querySelector("span").textContent = "Make change";
      if (playbackStateCaptured && restorePlayback && !audio.isActive) await audio.start();
    }
  }

  async function submitPrompt(event) {
    event.preventDefault();
    if (state.promptPending) return;
    const submittedText = elements.promptInput.value;
    const prompt = submittedText.trim();
    if (!prompt) return;
    const pending = {
      operationId: operationId(),
      prompt,
      submittedText,
      start: state.selectionStart,
      end: state.selectionEnd,
      acceptedJob: null,
    };
    persistPendingEdit(pending);
    await runPendingEdit(pending, true);
  }

  function undo() {
    return enqueueProjectMutation(applyUndo);
  }

  async function applyUndo() {
    try {
      await replaceProject(
        async () => {
          state.project = await api("/api/undo", { method: "POST" });
          renderProject();
        },
        { resumePlayback: false },
      );
      showToast("Last change undone");
    } catch (error) {
      showError(error, "undoing a project change");
    }
  }

  async function reset() {
    if (!window.confirm("Reset to the original demo arrangement? You can still undo this.")) return;
    await enqueueProjectMutation(applyReset);
  }

  async function applyReset() {
    try {
      await replaceProject(
        async () => {
          state.project = await api("/api/reset", { method: "POST" });
          state.selectionStart = 8;
          state.selectionEnd = 16;
          renderProject();
        },
        { preservePosition: false, resumePlayback: false },
      );
      showToast("Demo arrangement restored");
    } catch (error) {
      showError(error, "resetting the project");
    }
  }

  function setView(view) {
    const views = [
      { name: "ai", button: elements.aiModeButton, panel: elements.aiModePanel },
      { name: "advanced", button: elements.advancedButton, panel: elements.advancedDrawer },
      { name: "debug", button: elements.debugButton, panel: elements.debugPanel },
    ];
    if (!views.some((entry) => entry.name === view)) return;
    state.activeView = view;
    for (const entry of views) {
      const active = entry.name === view;
      entry.button.classList.toggle("is-active", active);
      entry.button.setAttribute("aria-selected", String(active));
      entry.button.tabIndex = active ? 0 : -1;
      entry.panel.hidden = !active;
      entry.panel.inert = !active;
    }
    elements.advancedDrawer.classList.toggle("is-open", view === "advanced");
    if (view === "debug") renderDebug();
    if (view === "ai" && state.project) {
      renderSelection();
      renderPlayhead();
    }
    window.scrollTo(0, 0);
  }

  function openAdvanced() {
    setView("advanced");
  }

  function closeAdvanced() {
    setView("ai");
    elements.aiModeButton.focus();
  }

  function openDebug() {
    setView("debug");
    void loadGeminiSessions();
  }

  function skipToTimeline(event) {
    event.preventDefault();
    setView("ai");
    elements.timelinePanel.focus({ preventScroll: true });
    elements.timelinePanel.scrollIntoView({ block: "start" });
  }

  function handleViewTabKey(event) {
    const tabs = [elements.aiModeButton, elements.advancedButton, elements.debugButton];
    const current = tabs.indexOf(event.currentTarget);
    let next = current;
    if (event.key === "ArrowRight") next = (current + 1) % tabs.length;
    else if (event.key === "ArrowLeft") next = (current + tabs.length - 1) % tabs.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = tabs.length - 1;
    else return;
    event.preventDefault();
    tabs[next].click();
    tabs[next].focus();
  }

  function debugReport() {
    const project = state.project;
    const lines = [
      "DAW-AI debug report",
      `Generated: ${new Date().toISOString()}`,
      `URL: ${window.location.href}`,
      `User agent: ${navigator.userAgent}`,
      `Viewport: ${window.innerWidth}x${window.innerHeight} at ${window.devicePixelRatio || 1}x`,
      `Network: ${navigator.onLine ? "online" : "offline"}`,
      `View: ${state.activeView}`,
      `Audio: ${audio.playbackState}; context ${audio.context?.state || "not initialized"}`,
      `AI edit: ${state.promptPending ? "pending" : "idle"}`,
      `Gemini sessions: ${state.geminiSessions.length} retained locally`,
      `Selection: ${state.selectionStart.toFixed(1)}s - ${state.selectionEnd.toFixed(1)}s`,
    ];
    if (project) {
      lines.push(
        `Project: ${project.name}`,
        `Project version: ${project.version}`,
        `Arrangement: ${project.bpm} BPM; ${project.duration}s; ${project.tracks.length} tracks; ${project.edits.length} edits`,
      );
    } else {
      lines.push("Project: unavailable");
    }
    lines.push("", "Recent Gemini sessions:");
    if (state.geminiSessions.length === 0) {
      lines.push("None found.");
    } else {
      for (const session of state.geminiSessions.slice(0, 10)) {
        lines.push(
          `${new Date(Number(session.createdAt) || 0).toISOString()} [${session.status || "unknown"}] ` +
            `${session.appliedSteps || 0} edit actions, ${session.audioListens || 0} listens: ${session.prompt || ""}`,
        );
      }
    }
    lines.push("", "Recent browser errors and warnings:");
    if (state.clientIssues.length === 0) {
      lines.push("None recorded in this browser session.");
    } else {
      for (const issue of state.clientIssues) {
        lines.push(`${issue.time} [${issue.level.toUpperCase()}] ${issue.context}: ${issue.message}`);
      }
    }
    lines.push("", "Backend warnings and errors are written to the DAW-AI server's stderr.");
    return lines.join("\n");
  }

  function renderDebug() {
    elements.debugReport.value = debugReport();
    if (state.geminiSessions.length === 0) {
      elements.geminiSessionList.innerHTML = '<div class="empty-log">No Gemini sessions recorded yet.</div>';
      return;
    }
    elements.geminiSessionList.innerHTML = state.geminiSessions
      .slice(0, 20)
      .map(
        (session) => `<article class="gemini-session-item">
          <div><strong>${escapeHtml(new Date(Number(session.createdAt) || 0).toLocaleString())}</strong>
          <span>${escapeHtml(session.status || "unknown")} &middot; ${Number(session.appliedSteps) || 0} actions &middot; ${Number(session.audioListens) || 0} listens</span></div>
          <p>${escapeHtml(session.prompt || "Untitled edit")}</p>
        </article>`,
      )
      .join("");
  }

  async function copyDebugReport() {
    renderDebug();
    try {
      if (!navigator.clipboard?.writeText) throw new Error("Clipboard API unavailable");
      await navigator.clipboard.writeText(elements.debugReport.value);
    } catch (_error) {
      elements.debugReport.focus();
      elements.debugReport.select();
      if (!document.execCommand("copy")) {
        showToast("Select the diagnostic report and copy it manually.", true);
        return;
      }
      elements.debugReport.setSelectionRange(0, 0);
    }
    showToast("Diagnostic report copied");
  }

  function clearDebugIssues() {
    state.clientIssues = [];
    renderDebug();
    showToast("Browser issues cleared");
  }

  async function loadGeminiSessions() {
    try {
      const response = await api("/api/gemini-sessions");
      state.geminiSessions = Array.isArray(response.sessions) ? response.sessions : [];
      renderDebug();
    } catch (error) {
      showError(error, "loading Gemini sessions");
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
  elements.skipLink.addEventListener("click", skipToTimeline);
  elements.aiModeButton.addEventListener("click", () => setView("ai"));
  elements.advancedButton.addEventListener("click", openAdvanced);
  elements.debugButton.addEventListener("click", openDebug);
  [elements.aiModeButton, elements.advancedButton, elements.debugButton].forEach((button) => {
    button.addEventListener("keydown", handleViewTabKey);
  });
  elements.closeAdvanced.addEventListener("click", closeAdvanced);
  elements.channelCreator.addEventListener("submit", (event) => {
    event.preventDefault();
    void changeChannel("add", { role: elements.channelRole.value });
  });
  elements.copyDebug.addEventListener("click", () => void copyDebugReport());
  elements.clearDebug.addEventListener("click", clearDebugIssues);
  elements.refreshGeminiSessions.addEventListener("click", () => void loadGeminiSessions());
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
  window.addEventListener("error", (event) => {
    reportClientIssue("error", event.error || event.message, "uncaught browser error");
  });
  window.addEventListener("unhandledrejection", (event) => {
    reportClientIssue("error", event.reason, "unhandled browser promise rejection");
  });
  window.addEventListener("resize", () => {
    renderSelection();
    renderPlayhead();
    renderDebug();
  });
  document.addEventListener("keydown", (event) => {
    const nativeSpaceSelector = "textarea, input, button, select, summary, a[href], [contenteditable='true']";
    const nativeSpaceControl =
      event.target.closest?.(nativeSpaceSelector) ?? document.activeElement?.closest?.(nativeSpaceSelector);
    if (event.code === "Space" && !nativeSpaceControl) {
      event.preventDefault();
      void audio.toggle();
    }
  });

  async function initialize() {
    const serverAudioToken = new URLSearchParams(window.location.search).get("server-audio-render");
    if (serverAudioToken) {
      await renderServerAudio(serverAudioToken);
      return;
    }
    const pending = readPendingEdit();
    if (pending) {
      if (!elements.promptInput.value) elements.promptInput.value = pending.submittedText;
      showPendingEdit("Reconnecting to the active AI edit");
    }
    await loadProject();
    await loadGeminiSessions();
    if (pending && state.project) await runPendingEdit(pending, false);
  }

  void initialize();
})();
