#!/usr/bin/env python3
"""Check the public static-site contract; accepts one or more origin/CDN base URLs."""

import argparse
import sys
from urllib.error import HTTPError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


OPENER = build_opener(NoRedirect)
CONTENT_CACHE = 'public, max-age=0, s-maxage=60, must-revalidate'
ASSET_CACHE = 'public, max-age=300, s-maxage=300'
IMAGE_CACHE = 'public, max-age=3600, s-maxage=86400'


def fetch(base_url, path):
    request = Request(base_url.rstrip('/') + path, headers={'User-Agent': 'rpi-website-verify/1'})
    try:
        response = OPENER.open(request, timeout=15)
    except HTTPError as error:
        response = error
    with response:
        return response.code, response.headers, response.read()


def check(base_url, path, status, cache, marker=None, location=None, content_type=None):
    actual_status, headers, body = fetch(base_url, path)
    label = base_url.rstrip('/') + path
    if actual_status != status:
        raise AssertionError(f'{label}: expected HTTP {status}, got {actual_status}')
    expected_cache = {part.strip() for part in cache.split(',')}
    actual_cache = {part.strip().lower() for part in headers.get('Cache-Control', '').split(',')}
    if actual_cache != expected_cache:
        raise AssertionError(f'{label}: unexpected Cache-Control: {headers.get("Cache-Control")}')
    if marker is not None and marker not in body:
        raise AssertionError(f'{label}: expected content not found')
    if location is not None and headers.get('Location') != location:
        raise AssertionError(f'{label}: unexpected redirect: {headers.get("Location")}')
    if content_type is not None and content_type not in headers.get('Content-Type', ''):
        raise AssertionError(f'{label}: unexpected Content-Type: {headers.get("Content-Type")}')
    print(f'OK {actual_status} {label}')


def verify(base_url):
    parsed = urlsplit(base_url)
    if parsed.scheme not in ('http', 'https') or not parsed.netloc or parsed.query or parsed.fragment:
        raise ValueError(f'Expected an HTTP(S) base URL without query/fragment: {base_url}')
    check(base_url, '/', 200, CONTENT_CACHE, marker=b'bigfish1913/pi-rust', content_type='text/html')
    check(base_url, '/index.html?lang=en', 200, CONTENT_CACHE, marker=b'bigfish1913/pi-rust')
    check(base_url, '/guide.html?doc=extension-authoring&lang=zh', 200, CONTENT_CACHE)
    check(base_url, '/data/packages.json', 200, CONTENT_CACHE, marker=b'rpi-cli', content_type='application/json')
    check(base_url, '/data/packages-meta.json', 200, CONTENT_CACHE, content_type='application/json')
    check(base_url, '/content/user-guide.html', 200, CONTENT_CACHE, content_type='text/html')
    check(base_url, '/styles.css?v=cdn-verify', 200, ASSET_CACHE, content_type='text/css')
    check(base_url, '/i18n.js?v=cdn-verify', 200, ASSET_CACHE, marker=b'initI18n')
    check(base_url, '/favicon.svg', 200, IMAGE_CACHE, content_type='image/svg+xml')
    check(base_url, '/user-guide.md', 302, 'no-store', location='/guide.html?doc=user-guide')
    for path in (
        '/.rpi', '/.rpi/todo.json', '/.deploy/website.tar.gz', '/.git/config',
        '/nested/.rpi/session.json', '/.RPI/todo.json', '/%2erpi/todo.json',
        '/__rpi_missing__.css', '/data/__rpi_missing__.json', '/__rpi_missing__.html',
    ):
        check(base_url, path, 404, 'no-store')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('base_urls', nargs='+', help='e.g. https://rpi.laofu.online https://rpi.bigfisharc.cn')
    args = parser.parse_args()
    try:
        for base_url in args.base_urls:
            verify(base_url)
    except (AssertionError, OSError, ValueError) as error:
        print(f'FAIL: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
