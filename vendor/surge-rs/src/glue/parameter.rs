use super::hell_ffi;

/* WARNING!
 * this is a hefty reimplementation.
 * why? because it's easier than trying to apply macros to bind generations (to standardize).
 * this puts a bit of a wrench on maintainability, given that it makes the library self-sufficient.
 * like, a little bit of a wrench. it puts the two tips into the engine gears, you know?
 * i also realized that the architecture used by surge is a MESS i cannot use like this.
 * still, enum compatibility is kept, as i suspect that'll be useful later on.
 * ...plus, i have no reason not to add a few (dozen) reprs.
 * (-:
 */

// TODO: finish this...
// STOP! no. do not.
// it goes unused.
/*
#[repr(C)]
pub enum ControlType {
    //----------------------------------------------------| None.
    None                                            = 0,
    //----------------------------------------------------| Percent.
    Percent                                         = 1,
    PercentDeactivatable                            = 2,    // crazy word.
    PercentWithStringDeformHook                     = 3,
    PercentBipolar                                  = 5,
    PercentBipolarPan                               = 157,
    PercentBipolarStereo                            = 6,
    PercentBipolarStringBalance                     = 7,
    PercentBipolarWithStringFilterHook              = 8,
    PercentBipolarWithDynamicUnipolarFormatting     = 9,
    PercentWithExtendToBipolar                      = 10,
    PercentWithExtendToBipolarStaticDefault         = 11,
    PercentOscDrift                                 = 143,
    //----------------------------------------------------|
    NoiseColor                                      = 12,
    //----------------------------------------------------| Pitch.
    Pitch                                           = 17,
    PitchOctave                                     = 14,
    PitchSemi7bp                                    = 15,   // QUERY DEVS.
    PitchSemi7bpAbsolutable                         = 16,   // QUERY DEVS.
    PitchExtendableVeryLowMinval                    = 18,
    PitchbendDepth                                  = 21,
    //----------------------------------------------------| FM.
    FmConfig                                        = 72,
    FmRatio                                         = 19,
    FmRatioInt                                      = 20,
    SineFmLegacy                                    = 99,
    //----------------------------------------------------|
    SyncPitch                                       = 22,   // QUERY DEVS.
    Amplitude                                       = 23,   // QUERY DEVS.
    Reverbshape                                     = 24,   // QUERY DEVS.
    //----------------------------------------------------| Decibel.
    Decibel                                         = 25,   // QUERY DEVS.
    DecibelNarrow                                   = 26,   // QUERY DEVS.
    DecibelNarrowExtendable                         = 27,   // QUERY DEVS.
    DecibelNarrowShortExtendable                    = 28,   // QUERY DEVS.
    DecibelExtraNarrow                              = 29,   // QUERY DEVS.
    DecibelAttenuation                              = 30,   // QUERY DEVS.
    DecibelAttenuationClipper                       = 31,   // QUERY DEVS.
    DecibelAttenuationLarge                         = 32,   // QUERY DEVS.
    DecibelAttenuationPlus12                        = 33,   // QUERY DEVS.
    DecibelFmdepth                                  = 34,   // QUERY DEVS.
    DecibelExtendable                               = 35,   // QUERY DEVS.
    DecibelDeactivatable                            = 36,   // QUERY DEVS.
    //----------------------------------------------------| Frequency.
    FreqAudible                                     = 37,   // QUERY DEVS.
    FreqAudibleDeactivatable                        = 38,   // QUERY DEVS.
    FreqAudibleDeactivatableHp                      = 39,   // QUERY DEVS.
    FreqAudibleDeactivatableLp                      = 40,   // QUERY DEVS.
    FreqAudibleWithTunability                       = 41,   // QUERY DEVS.
    FreqAudibleVeryLowMinVal                        = 42,   // QUERY DEVS.
    FreqAudibleFm3Extendable                        = 43,   // QUERY DEVS.
    FreqMod                                         = 44,   // QUERY DEVS.
    FreqHpf                                         = 45,   // QUERY DEVS.
    FreqShift                                       = 46,   // QUERY DEVS.
    FreqFm2Offset                                   = 47,   // QUERY DEVS.
    FreqVocoderLow                                  = 48,   // QUERY DEVS.
    FreqVocoderHigh                                 = 49,   // QUERY DEVS.
    FreqRingmod                                     = 141,  // QUERY DEVS.
    //----------------------------------------------------|
    Bandwidth                                       = 50,
    //----------------------------------------------------| Env.
    EnvTime                                         = 51,
    EnvTimeDeformable                               = 52,
    EnvTimeDeactivatable                            = 53,
    EnvTimeLfoDecay                                 = 54,
    EnvShape                                        = 55,
    EnvShapeAttack                                  = 56,
    EnvMode                                         = 57,
    //----------------------------------------------------| Portamento (SCE).
    PortamentoTime                                  = 61,
    //----------------------------------------------------| LFO.
    LfoRate                                         = 62,
    LfoRateDeactivatable                            = 63,
    LfoDeform                                       = 64,
    LfoType                                         = 65,
    LfoTriggerMode                                  = 66,
    //----------------------------------------------------|
    Detuning                                        = 67,   // QUERY DEVS.
    Fbconfig                                        = 71,   // QUERY DEVS.
    ModernTrimix                                    = 142,  // QUERY DEVS.
    //----------------------------------------------------| Filter (SCE).
    FilterType                                      = 73,
    FilterSubtype                                   = 74,
    FilterFeedback                                  = 110,
    //----------------------------------------------------| Waveshaper (SCE).
    WaveshaperType                                  = 75,
    //----------------------------------------------------|
    Wt2Window                                       = 76,   // QUERY DEVS.
    //----------------------------------------------------| Oscillator (SCE).
    OscType                                         = 68,
    OscCount                                        = 77,
    OscSpread                                       = 78,
    OscSpreadBipolar                                = 79,
    OscRoute                                        = 94,
    OscFeedback                                     = 111,
    OscFeedbackNegative                             = 112,  // QUERY DEVS.
    //----------------------------------------------------| Sine (OSC).
    SineOscMode                                     = 97,
    //----------------------------------------------------| String (OSC).
    StringoscExcitationModel                        = 144,
    //----------------------------------------------------| Twist (OSC).
    TwistEngine                                     = 146,
    TwistAuxMix                                     = 13,
    //----------------------------------------------------| Alias (OSC).
    AliasWave                                       = 149,
    AliasMask                                       = 150,
    AliasBits                                       = 151,
    //----------------------------------------------------| Scene.
    SceneMode                                       = 80,
    SceneSelection                                  = 81,
    //----------------------------------------------------| Poly.
    PolyMode                                        = 82,
    PolyLimit                                       = 83,
    //----------------------------------------------------| Midi.
    MidiKey                                         = 84,
    MidiKeyOrChannel                                = 85,
    //----------------------------------------------------| Bool.
    Bool                                            = 86,
    BoolRelativeSwitch                              = 87,
    BoolLinkSwitch                                  = 88,
    BoolKeytrack                                    = 89,
    BoolRetrigger                                   = 90,
    BoolUnipolar                                    = 91,
    BoolMute                                        = 92,
    BoolSolo                                        = 93,
    //----------------------------------------------------| Misc..
    StereoWidth                                     = 95,
    Character                                       = 96,
    RingmodSineOscMode                              = 98,
    CountedsetPercent                               = 100,
    CountedsetPercentExtendable                     = 101,
    CountedsetPercentExtendableWtdeform             = 102,
    VocoderBandCount                                = 103,
    DistortionWaveshape                             = 104,
    //----------------------------------------------------| Flanger.
    FlangerPitch                                    = 105,
    FlangerMode                                     = 106,
    FlangerVoices                                   = 108,
    FlangerSpacing                                  = 109,
    //----------------------------------------------------|
    ChorusModtime                                   = 113,
    Percent200                                      = 114,  // QUERY DEVS.
    RotaryDrive                                     = 115,
    SendLevel                                       = 116,
    PhaserStages                                    = 117,
    LfoAmplitude                                    = 118,
    VocoderModMode                                  = 119,
    //----------------------------------------------------|
    AmplitudeClipper                                = 124,
    DecibelNarrowDeactivatable                      = 126,
    DecibelExtraNarrowDeactivatable                 = 127,
    //----------------------------------------------------| FX (SCE).
    FxType                                          = 69,
    FxBypass                                        = 70,
    FxLfowave                                       = 107,
    //----------------------------------------------------| Delay (FX)
    DelayFeedbackClippingModes                      = 4,    // how do i read this. edit: did it.
    DelayModulationtime                             = 58,
    //----------------------------------------------------| Reverb (FX).
    ReverbTime                                      = 59,
    ReverbPreDelayTime                              = 60,
    //----------------------------------------------------| EQ (FX).
    FreqResonBand1                                  = 128,
    FreqResonBand2                                  = 129,
    FreqResonBand3                                  = 130,
    //----------------------------------------------------|
    ResonMode                                       = 131,
    EnvtimeLinkableDelay                            = 132,
    ResonResExtendable                              = 133,
    //----------------------------------------------------| CHOW (FX).
    ChowRatio                                       = 134,
    //----------------------------------------------------| NIMBUS (FX).
    NimbusMuode                                      = 135,
    NimbusQuality                                   = 136,
    //----------------------------------------------------|
    Pitch4oct                                       = 137,  // QUERY DEVS.
    FloatToggle                                     = 138,  // QUERY DEVS.
    //----------------------------------------------------|
    CompAttackMs                                    = 139,  // QUERY DEVS.
    CompReleaseMs                                   = 140,  // QUERY DEVS.
    //----------------------------------------------------| Phaser (FX).
    PhaserSpread                                    = 125,
    //----------------------------------------------------| Ensemble (FX).
    EnsembleStages                                  = 147,
    EnsembleClockRate                               = 148,
    EnsembleLfoRate                                 = 145,
    //----------------------------------------------------| Tape (FX).
    TapeDrive                                       = 152,
    TapeMicrons                                     = 153,
    TapeSpeed                                       = 154,
    //----------------------------------------------------|
    Lfophaseshuffle                                 = 155,  // QUERY DEVS.
    MsCodec                                         = 156,  // QUERY DEVS.
    AmplitudeRingmod                                = 159,  // QUERY DEVS.
    //----------------------------------------------------| Bonsai (FX).
    BonsaiBassBoost                                 = 160,
    BonsaiSaturationFilter                          = 161,
    BonsaiSaturationMode                            = 162,
    BonsaiNoiseMode                                 = 163,
    //----------------------------------------------------| Convolution (FX).
    ConvolutionDelay                                = 167,
    //----------------------------------------------------| FloatyWarp (FX?).
    FloatyWarpTime                                  = 164,
    FloatyDelayTime                                 = 165,
    FloatyDelayPlayrate                             = 166,
    //----------------------------------------------------| Spring reverb (FX).
    SpringDecay                                     = 158,
    //----------------------------------------------------| Airwindows (FX).
    AirwindowsFx                                    = 120,
    AirwindowsParam                                 = 121,
    AirwindowsParamBipolar                          = 122,
    AirwindowsParamIntegral                         = 123,
}

pub enum ControlGroup {
    Global      = 0,
    Oscillator  = 2,
    Mixer       = 3,    // Mixer or Mix?
    Filter      = 4,
    Envelope    = 5,
    Lfo         = 6,
    Fx          = 7,
}

#[repr(C)]
pub enum SceneMode {
    Single,
    Split,
    Dual,
    ChannelSplit,
}

#[repr(C)]
pub enum PlayMode {
    Poly,
    Mono,
    MonoSt,
    MonoFp,
    MonoStFp,
    Latch,
}

#[repr(C)]
pub enum PortamentoCruve {
    Log = -1,
    Linear = 0,
    Exponential = 1,
}

// TODO: evaluate this type thing.
#[repr(C)]
pub enum DeformType {
    Type1,
    Type2,
    Type3,
    Type4,
}

#[repr(C)]
pub enum NoiseColourChannels {
    Stereo,
    Mono,
}

#[repr(C)]
pub enum NoiseColourValue {
    Legacy,
    Tilt,
}

// TODO: figure out what the cryptic names (and kill the author).
#[repr(C)]
pub enum CombinatorMode {
    Ring,
    Bullshit1,
    Bullshit2,
    Bullshit3,
    Bullshit4,
    BullShit5,
    BullShit6,
    BullShit7,
    BullShit8,
    BullShit9,
    Bullshit3Legacy,
    Bullshit4Legacy,
}

#[repr(C)]
pub enum LfoTriggerMode {
    Freerun,
    Keytrigger,
    Random,
}

#[repr(C)]
pub enum CharacterMode {
    Warm,
    Neutral,
    Bright,
}

// aligned to ui.
#[repr(C)]
pub enum OscillatorType {
    Classic     = 0,
    Modern      = 8,
    // --------------|
    Wavetable   = 2,
    Window      = 7,
    // --------------|
    Sine        = 1,
    FM2         = 6,
    FM3         = 5,
    // --------------|
    String      = 9,
    Twist       = 10,
    // --------------|
    Alias       = 11,
    ShNoise     = 3,
    // --------------|
    AudioInput  = 4,        // i don't want to do this one...
}

// aligned to my own tastes (think hanoi tower).
#[repr(C)]
pub enum FxSlotPosition {
    SceneAInsert1   = 0,
    SceneAInsert2   = 1,
    SceneAInsert3   = 8,
    SceneAInsert4   = 9,
    SceneBInsert1   = 2,
    SceneBInsert2   = 3,
    SceneBInsert3   = 10,
    SceneBInsert4   = 11,
    Send1           = 4,
    Send2           = 5,
    Send3           = 12,
    Send4           = 13,
    Global1         = 6,
    Global2         = 7,
    Global3         = 14,
    Global4         = 15,
}

// TODO: look into what this one's for. remove if useless.
#[repr(C)]
pub enum FxChain {
    SceneA,
    SceneB,
    Send,
    Global,
}

// here comes a big one.
// aligned to ui.
#[repr(C)]
pub enum FxType {
    Off                 = 0,
    Eq                  = 6,
    Exciter             = 19,
    GraphicEq           = 16,
    Resonator           = 17,
    // ----------------------|
    Chow                = 18,
    Bonsai              = 28,
    Distortion          = 5,
    Neuron              = 15,
    Tape                = 23,
    Waveshaper          = 25,
    // ----------------------|
    Combulator          = 21,
    FrequencyShifter    = 7,
    Nimbus              = 22,
    Ringmod             = 13,   // Ringmod or RingModulator?
    Treemonster         = 24,
    Vocoder             = 10,
    // ----------------------|
    Chorus              = 9,
    Ensemble            = 20,
    Flanger             = 12,
    Phaser              = 3,
    RotarySpeaker       = 4,
    // ----------------------|
    Convolution         = 31,
    FloatyDelay         = 30,
    Delay               = 1,
    Reverb1             = 2,
    Reverb2             = 11,
    SpringReverb        = 27,
    // ----------------------|
    Airwindows          = 14,
    AudioInput          = 29,
    Conditioner         = 8,
    MidSideTool         = 26,
}

#[repr(C)]
pub enum FxBypass {
    All,            // All or AllFx?
    NoSends,
    SceneFxOnly,    // SceneFxOnly or SceneOnly?
    NoFx,           // NoFx or None or No?
}

#[repr(C)]
pub enum FilterConfig {
    Serial1,
    Serial2,
    Serial3,
    Dual1,
    Dual2,
    Stereo,
    Ring,
    Wide,
}

// TODO: come up with something better.
#[repr(C)]
pub enum FmRouting {
    Off,
    TwoModOne,
    ThreeModTwoModOne,
    TwoThreeModOne,
}

// TODO: check and align with ui.
#[repr(C)]
pub enum LfoType {
    Sine,
    Triange,
    Square,
    Ramp,
    Noise,
    SampleAndHold,
    Envelope,
    StepSequence,
    Mseg,
    Formula,
}

// TODO: see if this is okay to write like this.
#[repr(C)]
pub enum Adsr {
    Amp,
    Filter,
}

#[repr(C)]
pub enum MonoPedalMode {
    HoldAll,    // HoldAll or HoldAllNotes?
    ReleaseIfOthersHeld,
}

// hanoi.
#[repr(C)]
pub enum MonoVoicePriorityMode {
    Latest = 1,
    Highest = 2,
    Lowerst = 3,
    LatestLegacy = 0,
}

// good? yeah. yeah, it is.
#[repr(C)]
pub enum MonoVoiceEnvelopeMode {
    RestartFromZero,
    RestartFromlatest,
}

// good? i think so. even if differing.
pub enum PolyVoiceRepeatedKeyMode {
    NewVoiceForKey,
    OneVoiceForKey,
}*/

pub struct Parameter {
    pub ptr: *mut hell_ffi::Parameter,
}

impl Parameter {
    pub fn new() -> Self {
        Self { ptr: unsafe { &mut hell_ffi::Parameter::new() } }
    }
}

impl Drop for Parameter {
    fn drop(&mut self) {
        unsafe { hell_ffi::destroy_parameter(self.ptr); }
    }
}

/* stuff to bring in:
 * O scene_mode
 * X scene_mode_names
 * O play_mode
 * X play_mode_names
 * O porta_curve
 * O deform_type
 * O NoiseColorChannels         written wrong.
 * O NoiseColorValue            written wrong.
 * ? CombinatorMode
 * X combinator_mode_names
 * O lfo_trigger_mode
 * X lfo_trigger_mode_names
 * O character_mode
 * X character_names
 * O osc_type
 * X osc_type_names
 * X osc_type_shortnames
 * X window_names
 * X uses_wavetabledata
 * O fxslot_positions
 * X fxslot_order
 * X fxchains
 * X fxslot_names
 * X fxslot_longnames
 * X fxslot_shortnames
 * X fxslot_shortoscname        i thought it meant something else.
 * O fx_type
 * X fx_type_names
 * X fx_type_shortnames
 * X fx_type_acronyms
 * O fx_bypass
 * X fx_bypass_names
 * O filter_config
 * X fbc_names
 * O fm_routing
 * X fmr_names
 * O lfo_type
 * X lt_names
 * X lt_num_deforms
 * X env_mode
 * X em_names
 * O adsr_purpose
 * O MonoPedalMode
 * O MonoVoicePriorityMode
 * O MonoVoiceEnvelopeMode
 * O PolyVoiceRepeatedKeyMode
 * X MidiKeyState
 * X MidiChannelState
 * X ArbitraryBlockStorage
 * X OscillatorStorage
 * X FilterStorage
 * X WaveshaperStorage
 * X ADSRStorage
 * X LFOStorage
 * X FxStorage
 * X SurgeSceneStorage
 * X StepSequencerStorage
 * X MSEGStorage
 * X FormulaModulatorStorage
 * X DAWExtraStateStorage
 * X PatchTuningStorage
 * X SurgePatch
 *
 * X pdata
 * X valtypes
 * X ctrltypes
 */

// doing wandering ghost behaviour right now...
