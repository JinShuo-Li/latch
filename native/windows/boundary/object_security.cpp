#include "object_security.h"

#include <utility>
namespace latch {
constexpr SECURITY_INFORMATION security_parts = OWNER_SECURITY_INFORMATION |
                                                GROUP_SECURITY_INFORMATION |
                                                DACL_SECURITY_INFORMATION;
std::wstring descriptor_text(PSECURITY_DESCRIPTOR sd) {
  LPWSTR text = nullptr;
  if (!ConvertSecurityDescriptorToStringSecurityDescriptorW(
          sd, SDDL_REVISION_1, security_parts, &text, nullptr))
    fail(L"serialize recovery security descriptor");
  Local owner(text);
  return text;
}
Local descriptor(const std::wstring& text) {
  PSECURITY_DESCRIPTOR sd = nullptr;
  if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(
          text.c_str(), SDDL_REVISION_1, &sd, nullptr))
    fail(L"parse recovery ACL");
  return Local(sd);
}
PinnedObject::PinnedObject(const std::filesystem::path& input, DWORD access) {
  const auto path = std::filesystem::absolute(input).lexically_normal();
  require(path.is_absolute() && path.has_root_name() &&
              path.root_name().wstring().size() == 2,
          L"recovery requires an absolute local NTFS path");
  wchar_t fs[32]{};
  if (!GetVolumeInformationW(path.root_path().c_str(), nullptr, 0, nullptr,
                             nullptr, nullptr, fs, 32))
    fail(L"inspect recovery volume");
  require(std::wcscmp(fs, L"NTFS") == 0, L"recovery requires NTFS");
  auto current = path.root_path();
  const auto relative = path.relative_path();
  for (auto it = relative.begin(); it != relative.end(); ++it) {
    current /= *it;
    const bool last = std::next(it) == relative.end();
    Handle handle(CreateFileW(
        current.c_str(), last ? access : FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, nullptr));
    if (handle.value == INVALID_HANDLE_VALUE) {
      std::fwprintf(stderr, L"Recovery path unavailable: %ls\n",
                    current.c_str());
      fail(L"pin recovery object");
    }
    FILE_ATTRIBUTE_TAG_INFO tag{};
    if (!GetFileInformationByHandleEx(handle.value, FileAttributeTagInfo, &tag,
                                      sizeof(tag)))
      fail(L"inspect recovery object");
    require(!(tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT),
            L"recovery refuses reparse paths");
    if (last)
      object.value = std::exchange(handle.value, nullptr);
    else
      ancestors.push_back(std::move(handle));
  }
  if (!object.value) fail(L"cannot mutate a volume root", ERROR_ACCESS_DENIED);
}
std::wstring identity(HANDLE handle) {
  FILE_ID_INFO id{};
  FILE_BASIC_INFO basic{};
  if (!GetFileInformationByHandleEx(handle, FileIdInfo, &id, sizeof(id)) ||
      !GetFileInformationByHandleEx(handle, FileBasicInfo, &basic,
                                    sizeof(basic)))
    fail(L"read recovery file identity");
  wchar_t header[100]{};
  swprintf_s(header, L"%016llx:%016llx:%lx:", id.VolumeSerialNumber,
             static_cast<unsigned long long>(basic.CreationTime.QuadPart),
             basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY);
  std::wstring result(header);
  for (BYTE byte : id.FileId.Identifier) {
    wchar_t part[3]{};
    swprintf_s(part, L"%02x", byte);
    result += part;
  }
  return result;
}
std::wstring read_security(HANDLE handle) {
  PSECURITY_DESCRIPTOR sd = nullptr;
  const DWORD code = GetSecurityInfo(handle, SE_FILE_OBJECT, security_parts,
                                     nullptr, nullptr, nullptr, nullptr, &sd);
  if (code) fail(L"read recovery security", code);
  Local owner(sd);
  return descriptor_text(sd);
}
ObjectState PinnedObject::state() const {
  std::vector<wchar_t> name(32768);
  const auto length = GetFinalPathNameByHandleW(object.value, name.data(),
                                                static_cast<DWORD>(name.size()),
                                                FILE_NAME_NORMALIZED);
  if (!length || length >= name.size()) fail(L"read final recovery path");
  std::wstring path(name.data(), length);
  if (path.starts_with(L"\\\\?\\")) path.erase(0, 4);
  return {path, identity(object.value), read_security(object.value)};
}
std::wstring changed_dacl(const std::wstring& original, PACL acl,
                          bool protect) {
  auto sd = descriptor(original);
  SECURITY_DESCRIPTOR absolute{};
  if (!InitializeSecurityDescriptor(&absolute, SECURITY_DESCRIPTOR_REVISION))
    fail(L"initialize changed ACL");
  PSID owner = nullptr, group = nullptr;
  BOOL defaulted = FALSE;
  GetSecurityDescriptorOwner(sd.value, &owner, &defaulted);
  SetSecurityDescriptorOwner(&absolute, owner, defaulted);
  GetSecurityDescriptorGroup(sd.value, &group, &defaulted);
  SetSecurityDescriptorGroup(&absolute, group, defaulted);
  SECURITY_DESCRIPTOR_CONTROL control{};
  DWORD revision = 0;
  if (!GetSecurityDescriptorControl(sd.value, &control, &revision) ||
      !SetSecurityDescriptorDacl(&absolute, TRUE, acl, FALSE))
    fail(L"construct changed ACL");
  const auto flags = static_cast<SECURITY_DESCRIPTOR_CONTROL>(
      SE_DACL_PROTECTED | SE_DACL_AUTO_INHERITED | SE_DACL_AUTO_INHERIT_REQ);
  if (protect) control |= SE_DACL_PROTECTED;
  if (!SetSecurityDescriptorControl(&absolute, flags, control & flags))
    fail(L"changed ACL control");
  return descriptor_text(&absolute);
}
void write_security(HANDLE handle, const std::wstring& text) {
  auto sd = descriptor(text);
  SECURITY_DESCRIPTOR_CONTROL control{};
  DWORD revision = 0;
  if (!GetSecurityDescriptorControl(sd.value, &control, &revision))
    fail(L"read exact DACL control");
  // NtSetSecurityObject consumes AUTO_INHERIT_REQ to preserve the stored AI
  // flag. Unlike SetSecurityInfo, this does not rewrite/reorder inherited ACEs
  // or propagate a parent change to unjournaled children.
  if ((control & SE_DACL_AUTO_INHERITED) &&
      !SetSecurityDescriptorControl(sd.value, SE_DACL_AUTO_INHERIT_REQ,
                                    SE_DACL_AUTO_INHERIT_REQ))
    fail(L"preserve inherited ACL control");
  if (!SetKernelObjectSecurity(handle, DACL_SECURITY_INFORMATION, sd.value))
    fail(L"apply exact DACL");
  if (read_security(handle) != text)
    fail(L"ACL application was not exact; recovery retained",
         ERROR_INVALID_DATA);
}

}  // namespace latch
