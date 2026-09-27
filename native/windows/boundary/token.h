#pragma once
#include "common.h"
namespace latch {
Local parse_sid(const wchar_t* text);
std::wstring unique_sid_string();
std::vector<BYTE> token_info(HANDLE token, TOKEN_INFORMATION_CLASS type);
void prepare_default_dacl(HANDLE token, PSID sid);
Handle restrict_token(PSID write_sid);
}  // namespace latch
