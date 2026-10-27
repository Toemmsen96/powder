// Minimal WinHTTP client for the Powder overlay: talks to the Powder server's admin API over HTTPS, keeps a
// cookie jar (the `powder_admin` session cookie), and sets the headers the admin mod requires on writes
// (X-Powder-Admin: 1 and a same-origin Origin). TLS is validated against the prefix's certificate store,
// which already trusts Powder's local CA for the session - so no certificate checks are disabled here.
#pragma once
#include <string>
#include <map>

namespace pw {

struct HttpResponse {
    int status = 0;        // HTTP status, or 0 on a transport failure
    std::string body;
    std::string error;     // set when status == 0
    bool ok() const { return status >= 200 && status < 300; }
};

class HttpClient {
public:
    // base_url like "https://public-ubiservices.ubi.com" (optionally with :port)
    explicit HttpClient(const std::string &base_url);
    bool valid() const { return !whost_.empty(); }

    // method "GET"/"POST"; path like "/_powder/admin/api/players"; body for POST (JSON, may be empty).
    HttpResponse request(const std::string &method, const std::string &path, const std::string &body);
    HttpResponse get(const std::string &path) { return request("GET", path, std::string()); }
    HttpResponse post(const std::string &path, const std::string &body) { return request("POST", path, body); }

    void clear_cookies() { cookies_.clear(); }
    const std::string &host() const { return host_; }   // host[:port], for display

private:
    std::wstring whost_;             // host name (wide) for WinHttpConnect
    std::string host_;               // host[:port], for the Origin header and display
    unsigned short port_ = 0;
    bool https_ = true;
    std::map<std::string, std::string> cookies_;

    void store_set_cookie(const std::string &set_cookie);
    std::string cookie_header() const;
};

} // namespace pw
