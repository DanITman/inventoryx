<?php
/* InventoryX — forwards requests from the Unraid web UI to the InventoryX program.
 *
 * This file sits behind Unraid's own login (nginx auth_request) and request-forgery check
 * (local_prepend.php requires the CSRF token on every POST), so the program itself never
 * listens on the network.
 *
 * GPL-3.0-or-later.
 */
$socket = '/var/run/inventoryx.sock';
$method = $_SERVER['REQUEST_METHOD'] ?? 'GET';
$path = $_SERVER['PATH_INFO'] ?? '';

if ($method !== 'GET' && $method !== 'POST') {
  http_response_code(405);
  exit;
}
if ($path === '') {
  // Relative links in the page need the trailing slash.
  header('Location: ' . $_SERVER['SCRIPT_NAME'] . '/');
  exit;
}

$url = 'http://localhost' . implode('/', array_map('rawurlencode', explode('/', $path)));
$query = $_SERVER['QUERY_STRING'] ?? '';
if ($query !== '') $url .= '?' . $query;

$ch = curl_init($url);
curl_setopt_array($ch, [
  CURLOPT_UNIX_SOCKET_PATH => $socket,
  CURLOPT_RETURNTRANSFER => true,
  CURLOPT_TIMEOUT => 300,
]);
if ($method === 'POST') {
  curl_setopt($ch, CURLOPT_POST, true);
  curl_setopt($ch, CURLOPT_POSTFIELDS, file_get_contents('php://input'));
  curl_setopt($ch, CURLOPT_HTTPHEADER, ['Content-Type: application/json']);
}
$body = curl_exec($ch);

header('Cache-Control: no-store');
header('X-Content-Type-Options: nosniff');
if ($body === false) {
  http_response_code(503);
  header('Content-Type: application/json');
  echo json_encode(['error' => 'InventoryX is not running on the server.']);
  exit;
}
http_response_code(curl_getinfo($ch, CURLINFO_RESPONSE_CODE));
header('Content-Type: ' . (curl_getinfo($ch, CURLINFO_CONTENT_TYPE) ?: 'application/octet-stream'));

if ($path === '/' || $path === '/index.html') {
  // Hand the page the token Unraid expects on POST requests.
  $var = @parse_ini_file('/var/local/emhttp/var.ini') ?: [];
  $body = str_replace('window.IX_CSRF = "";',
                      'window.IX_CSRF = ' . json_encode($var['csrf_token'] ?? '') . ';', $body);
}
echo $body;
