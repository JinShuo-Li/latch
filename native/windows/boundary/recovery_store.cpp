#include "recovery_store.h"

#include <objbase.h>
#include <shlobj.h>

#include <utility>

#include "token.h"
namespace latch::recovery_store {
std::wstring current_user() {
  Handle token;
  if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token.value))
    fail(L"recovery user token");
  const auto bytes = token_info(token.value, TokenUser);
  LPWSTR text = nullptr;
  if (!ConvertSidToStringSidW(
          reinterpret_cast<const TOKEN_USER*>(bytes.data())->User.Sid, &text))
    fail(L"recovery user SID");
  Local owner(text);
  return text;
}
std::filesystem::path local_appdata() {
  PWSTR path = nullptr;
  const HRESULT result =
      SHGetKnownFolderPath(FOLDERID_LocalAppData, 0, nullptr, &path);
  if (FAILED(result)) fail(L"trusted LocalAppData", static_cast<DWORD>(result));
  std::filesystem::path output(path);
  CoTaskMemFree(path);
  return output;
}
std::wstring user_acl() {
  return L"O:" + current_user() + L"D:P(A;;FA;;;SY)(A;;FA;;;" + current_user() + L")";
}
uint32_t checksum(const std::vector<BYTE>& bytes) {
  uint32_t crc = 0xffffffffu;
  for (BYTE byte : bytes) {
    crc ^= byte;
    for (int i = 0; i < 8; ++i)
      crc = (crc >> 1) ^ ((crc & 1) ? 0xedb88320u : 0u);
  }
  return ~crc;
}
void append_number(std::vector<BYTE>& data, uint32_t value) {
  for (unsigned i = 0; i < 4; ++i)
    data.push_back(static_cast<BYTE>(value >> (i * 8)));
}
uint32_t number(const std::vector<BYTE>& data, size_t& offset) {
  if (offset + 4 > data.size())
    fail(L"truncated recovery record", ERROR_INVALID_DATA);
  uint32_t value = 0;
  for (unsigned i = 0; i < 4; ++i)
    value |= static_cast<uint32_t>(data[offset++]) << (i * 8);
  return value;
}
void durable_record(const std::filesystem::path& final,
                    const std::vector<std::wstring>& fields) {
  std::vector<BYTE> payload;
  append_number(payload, static_cast<uint32_t>(fields.size()));
  for (const auto& field : fields) {
    if (field.size() > 1024 * 1024)
      fail(L"recovery field too large", ERROR_BUFFER_OVERFLOW);
    append_number(payload, static_cast<uint32_t>(field.size()));
    const auto* bytes = reinterpret_cast<const BYTE*>(field.data());
    payload.insert(payload.end(), bytes,
                   bytes + field.size() * sizeof(wchar_t));
  }
  std::vector<BYTE> data;
  append_number(data,
                0x314a524c);  // LRJ1, little endian, bounded UTF-16 fields.
  append_number(data, static_cast<uint32_t>(payload.size()));
  append_number(data, checksum(payload));
  data.insert(data.end(), payload.begin(), payload.end());
  auto temporary = final;
  temporary += L".tmp";
  {
    auto security = descriptor(user_acl());
    SECURITY_ATTRIBUTES attributes{sizeof(attributes), security.value, FALSE};
    Handle file(CreateFileW(
        temporary.c_str(), GENERIC_WRITE, 0, &attributes, CREATE_NEW,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_WRITE_THROUGH, nullptr));
    if (file.value == INVALID_HANDLE_VALUE) fail(L"create recovery intent");
    DWORD written = 0;
    if (!WriteFile(file.value, data.data(), static_cast<DWORD>(data.size()),
                   &written, nullptr) ||
        written != data.size() || !FlushFileBuffers(file.value))
      fail(L"flush recovery intent");
  }
  // No replacement: each intent is immutable and has a unique sequence number.
  if (!MoveFileExW(temporary.c_str(), final.c_str(), MOVEFILE_WRITE_THROUGH))
    fail(L"publish recovery intent");
}
std::vector<std::wstring> read_record(const std::filesystem::path& path) {
  // The caller pins the protected journal directory and all ancestors.
  // Open this immutable record itself without following a reparse point.
  Handle file(CreateFileW(path.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr,
                          OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT,
                          nullptr));
  if (file.value == INVALID_HANDLE_VALUE) fail(L"open recovery record");
  FILE_ATTRIBUTE_TAG_INFO tag{};
  if (!GetFileInformationByHandleEx(file.value, FileAttributeTagInfo, &tag,
                                    sizeof(tag)) ||
      (tag.FileAttributes &
       (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY)))
    fail(L"invalid recovery record object", ERROR_INVALID_DATA);
  LARGE_INTEGER size{};
  if (!GetFileSizeEx(file.value, &size) || size.QuadPart < 16 ||
      size.QuadPart > 16 * 1024 * 1024)
    fail(L"invalid recovery record size", ERROR_INVALID_DATA);
  std::vector<BYTE> bytes(static_cast<size_t>(size.QuadPart));
  DWORD read = 0;
  if (!ReadFile(file.value, bytes.data(), static_cast<DWORD>(bytes.size()),
                &read, nullptr) ||
      read != bytes.size())
    fail(L"read recovery record");
  size_t offset = 0;
  if (number(bytes, offset) != 0x314a524c)
    fail(L"unsupported recovery journal", ERROR_INVALID_DATA);
  const uint32_t length = number(bytes, offset), crc = number(bytes, offset);
  std::vector<BYTE> payload(bytes.begin() + static_cast<ptrdiff_t>(offset),
                            bytes.end());
  if (length != payload.size() || checksum(payload) != crc)
    fail(L"corrupt recovery journal; preserve it for repair", ERROR_CRC);
  offset = 0;
  const auto count = number(payload, offset);
  if (count > 1024)
    fail(L"invalid recovery record fields", ERROR_INVALID_DATA);
  std::vector<std::wstring> fields;
  for (uint32_t i = 0; i < count; ++i) {
    const auto characters = number(payload, offset);
    if (characters > 1024 * 1024 ||
        offset + characters * sizeof(wchar_t) > payload.size())
      fail(L"invalid recovery string", ERROR_INVALID_DATA);
    std::wstring value(characters, 0);
    std::memcpy(value.data(), payload.data() + offset,
                characters * sizeof(wchar_t));
    if (value.find(L'\0') != std::wstring::npos)
      fail(L"invalid recovery path", ERROR_INVALID_DATA);
    offset += characters * sizeof(wchar_t);
    fields.push_back(std::move(value));
  }
  if (offset != payload.size())
    fail(L"recovery trailing data", ERROR_INVALID_DATA);
  return fields;
}

std::vector<std::vector<std::wstring>> read_records(
    const std::filesystem::path& path) {
  auto fields = read_record(path);
  if (fields.empty() || fields.front() != L"acl-batch-v1")
    return {std::move(fields)};
  require(fields.size() >= 2, L"invalid ACL batch record");
  uint32_t count = 0;
  require(!fields[1].empty(), L"invalid ACL batch record");
  for (wchar_t character : fields[1]) {
    require(character >= L'0' && character <= L'9',
            L"invalid ACL batch record");
    count = count * 10 + static_cast<uint32_t>(character - L'0');
    require(count > 0 && count <= 32, L"invalid ACL batch record");
  }
  require(fields.size() == 2 + static_cast<size_t>(count) * 5,
          L"invalid ACL batch record");
  std::vector<std::vector<std::wstring>> rows;
  rows.reserve(static_cast<size_t>(count));
  size_t offset = 2;
  for (uint32_t i = 0; i < count; ++i) {
    std::vector<std::wstring> row(fields.begin() + static_cast<ptrdiff_t>(offset),
                                  fields.begin() + static_cast<ptrdiff_t>(offset + 5));
    require(row[0] == L"acl", L"invalid ACL batch entry");
    rows.push_back(std::move(row));
    offset += 5;
  }
  return rows;
}

}  // namespace latch::recovery_store
