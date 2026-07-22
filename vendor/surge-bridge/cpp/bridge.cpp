#include "bridge.h"
#include "src/common/SurgeSynthesizer.h"

#include <cstdlib>

class ErrCork : public SurgeSynthesizer::PluginLayer {
  public:
    void surgeParameterUpdated(const SurgeSynthesizer::ID &id, float d) override {}
    void surgeMacroUpdated(long macroNum, float d) override {}
};

extern "C" {
    SurgeSynthesizer* create_engine(float sr) {
        static ErrCork layer;
        auto* surge = new SurgeSynthesizer(
            &layer, SurgeStorage::skipPatchLoadDataPathSentinel);

        surge->setSamplerate(sr);
        surge->time_data.tempo = 120;
        surge->time_data.ppqPos = 0;
        surge->storage.rngGen.g.seed(0);
        std::srand(0);

        return surge;
	}

    SurgePatch* create_patch() {
        SurgeStorage::SurgeStorageConfig sconf;
        sconf.scanWavetableAndPatches = false;
        sconf.createUserDirectory = false;

        auto* storage = new SurgeStorage(sconf);
        return new SurgePatch(storage);
    }

    void destroy_engine(SurgeSynthesizer* surge) {
        if (surge) delete surge;
    }

    void destroy_patch(SurgePatch* patch) {
        if (patch) delete patch;
    }

    void destroy_parameter(Parameter* parameter) {
        if (parameter) delete parameter;
    }
}
