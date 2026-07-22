#pragma once

class SurgeSynthesizer;
class SurgePatch;
class Parameter;

extern "C" {
	SurgeSynthesizer* create_engine(float sr);
	SurgePatch* create_patch();
    void destroy_engine(SurgeSynthesizer* surge);
    void destroy_patch(SurgePatch* patch);
    void destroy_parameter(Parameter* p);
}
