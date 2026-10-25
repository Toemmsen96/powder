#include "http.h"
#include <windows.h>
#include <winhttp.h>
#include <vector>

// (linked with -lwinhttp by native/build.sh)

namespace pw {

static std::wstring widen(const std::string &s) {
    if (s.empty()) return std::wstring();
    int n = MultiByteToWideChar(CP_UTF8, 0, s.data(), (int)s.size(), nullptr, 0);
    std::wstring w(n, 0);
    MultiByteToWideChar(CP_UTF8, 0, s.data(), (int)s.size(), &w[0], n);
    return w;
}
static std::string narrow(const std::wstring &w) {
    if (w.empty()) return std::string();
    int n = WideCharToMultiByte(CP_UTF8, 0, w.data(), (int)w.size(), nullptr, 0, nullptr, nullptr);
    std::string s(n, 0);
    WideCharToMultiByte(CP_UTF8, 0, w.data(), (int)w.size(), &s[0], n, nullptr, nullptr);
    return s;
}

HttpClient::HttpClient(const std::string &base_url) {
    std::wstring wurl = widen(base_url);
    URL_COMPONENTS uc;
    ZeroMemory(&uc, sizeof(uc));
    uc.dwStructSize = sizeof(uc);
    wchar_t host[256] = {0};
    uc.lpszHostName = host;
    uc.dwHostNameLength = 255;
    if (!WinHttpCrackUrl(wurl.c_str(), (DWORD)wurl.size(), 0, &uc)) return;
    whost_.assign(host, uc.dwHostNameLength);
    https_ = (uc.nScheme == INTERNET_SCHEME_HTTPS);
    port_ = uc.nPort ? uc.nPort : (https_ ? 443 : 80);
    host_ = narrow(whost_);
    if (!((https_ && port_ == 443) || (!https_ && port_ == 80)))
        host_ += ":" + std::to_string(port_);
}

void HttpClient::store_set_cookie(const std::string &set_cookie) {
    // keep only name=value (drop attributes after the first ';')
    size_t eq = set_cookie.find('=');
    if (eq == std::string::npos) return;
    size_t semi = set_cookie.find(';', eq);
    std::string name = set_cookie.substr(0, eq);
    std::string value = set_cookie.substr(eq + 1, (semi == std::string::npos ? std::string::npos : semi - eq - 1));
    // trim leading spaces on the name
    size_t a = name.find_first_not_of(" \t");
    if (a != std::string::npos) name = name.substr(a);
    if (!name.empty()) cookies_[name] = value;
}

std::string HttpClient::cookie_header() const {
    std::string out;
    for (const auto &kv : cookies_) {
        if (!out.empty()) out += "; ";
        out += kv.first + "=" + kv.second;
    }
    return out;
}

HttpResponse HttpClient::request(const std::string &method, const std::string &path, const std::string &body) {
    HttpResponse r;
    if (whost_.empty()) { r.error = "no server url"; return r; }

    HINTERNET sess = WinHttpOpen(L"PowderOverlay/1.0", WINHTTP_ACCESS_TYPE_NO_PROXY,
                                 WINHTTP_NO_PROXY_NAME, WINHTTP_NO_PROXY_BYPASS, 0);
    if (!sess) { r.error = "WinHttpOpen"; return r; }
    // modest timeouts: resolve 5s, connect 8s, send 8s, receive 15s
    WinHttpSetTimeouts(sess, 5000, 8000, 8000, 15000);

    HINTERNET conn = WinHttpConnect(sess, whost_.c_str(), port_, 0);
    if (!conn) { r.error = "WinHttpConnect"; WinHttpCloseHandle(sess); return r; }

    std::wstring wmethod = widen(method), wpath = widen(path);
    DWORD flags = https_ ? WINHTTP_FLAG_SECURE : 0;
    HINTERNET req = WinHttpOpenRequest(conn, wmethod.c_str(), wpath.c_str(), nullptr,
                                       WINHTTP_NO_REFERER, WINHTTP_DEFAULT_ACCEPT_TYPES, flags);
    if (!req) { r.error = "WinHttpOpenRequest"; WinHttpCloseHandle(conn); WinHttpCloseHandle(sess); return r; }

    // TEST ONLY: POWDER_OVERLAY_INSECURE skips certificate validation, for nettest.exe against a scratch
    // server reached by IP (name/CA won't match). Never set by powder-wrap; the real overlay validates
    // against the prefix's certificate store, which trusts Powder's CA for the session.
    if (https_ && getenv("POWDER_OVERLAY_INSECURE")) {
        DWORD sec = SECURITY_FLAG_IGNORE_UNKNOWN_CA | SECURITY_FLAG_IGNORE_CERT_CN_INVALID |
                    SECURITY_FLAG_IGNORE_CERT_DATE_INVALID | SECURITY_FLAG_IGNORE_CERT_WRONG_USAGE;
        WinHttpSetOption(req, WINHTTP_OPTION_SECURITY_FLAGS, &sec, sizeof(sec));
    }

    // headers the admin mod expects on writes, plus our cookie jar
    std::string headers = "Accept: application/json\r\nX-Powder-Admin: 1\r\n";
    if (method != "GET") {
        headers += "Content-Type: application/json\r\n";
        headers += "Origin: https://" + host_ + "\r\n";
    }
    std::string ck = cookie_header();
    if (!ck.empty()) headers += "Cookie: " + ck + "\r\n";
    std::wstring wheaders = widen(headers);

    BOOL sent = WinHttpSendRequest(req, wheaders.c_str(), (DWORD)-1L,
                                   body.empty() ? WINHTTP_NO_REQUEST_DATA : (LPVOID)body.data(),
                                   (DWORD)body.size(), (DWORD)body.size(), 0);
    if (sent) sent = WinHttpReceiveResponse(req, nullptr);
    if (!sent) {
        DWORD e = GetLastError();
        r.error = "send/receive failed (win32 " + std::to_string(e) + ")";
        WinHttpCloseHandle(req); WinHttpCloseHandle(conn); WinHttpCloseHandle(sess);
        return r;
    }

    DWORD code = 0, len = sizeof(code);
    WinHttpQueryHeaders(req, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                        WINHTTP_HEADER_NAME_BY_INDEX, &code, &len, WINHTTP_NO_HEADER_INDEX);
    r.status = (int)code;

    // collect any Set-Cookie headers (there may be several)
    for (DWORD idx = 0;; ++idx) {
        DWORD sz = 0;
        WinHttpQueryHeaders(req, WINHTTP_QUERY_SET_COOKIE, WINHTTP_HEADER_NAME_BY_INDEX,
                            WINHTTP_NO_OUTPUT_BUFFER, &sz, &idx);
        if (GetLastError() != ERROR_INSUFFICIENT_BUFFER || sz == 0) break;
        std::wstring buf(sz / sizeof(wchar_t), 0);
        DWORD used = sz;
        if (!WinHttpQueryHeaders(req, WINHTTP_QUERY_SET_COOKIE, WINHTTP_HEADER_NAME_BY_INDEX,
                                 &buf[0], &used, &idx))
            break;
        buf.resize(used / sizeof(wchar_t));
        store_set_cookie(narrow(buf));
    }

    // read the body
    for (;;) {
        DWORD avail = 0;
        if (!WinHttpQueryDataAvailable(req, &avail) || avail == 0) break;
        std::vector<char> chunk(avail);
        DWORD got = 0;
        if (!WinHttpReadData(req, chunk.data(), avail, &got) || got == 0) break;
        r.body.append(chunk.data(), got);
    }

    WinHttpCloseHandle(req);
    WinHttpCloseHandle(conn);
    WinHttpCloseHandle(sess);
    return r;
}

} // namespace pw
