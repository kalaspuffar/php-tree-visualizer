<?php

declare(strict_types=1);

require_once __DIR__ . '/lib/assert.php';
require_once __DIR__ . '/fixtures/make_config.php';
require_once __DIR__ . '/../../api/internal/config.php';

// --- 3.4 Load the documented shape ---------------------------------

$path = make_config([
    'auth' => [
        'token'        => 'tok-xyz-' . str_repeat('a', 32),
        'session_salt' => 'salt-' . str_repeat('b', 36),
    ],
    'storage' => ['data_dir' => '/var/lib/php-tree-viz'],
]);

Config::forgetCache();
$config = Config::load($path);

assert_eq(
    'tok-xyz-' . str_repeat('a', 32),
    $config->getString('auth', 'token'),
    'auth.token round-trips'
);
assert_eq(
    'salt-' . str_repeat('b', 36),
    $config->getString('auth', 'session_salt'),
    'auth.session_salt round-trips'
);
assert_eq(
    '/var/lib/php-tree-viz',
    $config->getString('storage', 'data_dir'),
    'storage.data_dir round-trips'
);
assert_eq(
    30,
    $config->getInt('storage', 'retention_days'),
    'storage.retention_days is an int'
);
assert_eq(
    67108864,
    $config->getInt('server', 'max_body_bytes'),
    'server.max_body_bytes is an int'
);
assert_eq(
    false,
    $config->getBool('server', 'tls', false),
    'server.tls defaults parse to bool false'
);

// Cache works
$again = Config::load($path);
assert_true($again === $config, 'same path returns cached instance');

// forgetCache() works
Config::forgetCache();
$fresh = Config::load($path);
assert_true($fresh !== $config, 'forgetCache forces re-parse');

// Sentinel collection
$sentinels = $fresh->logRedactionSentinels();
assert_true(in_array('tok-xyz-' . str_repeat('a', 32), $sentinels, true), 'token in sentinels');
assert_true(in_array('salt-' . str_repeat('b', 36), $sentinels, true), 'salt in sentinels');

@unlink($path);

// --- 3.5 Malformed-line cases --------------------------------------

$bad = tempnam(sys_get_temp_dir(), 'phptv_bad_');
// The value sentinel must not collide with the random temp filename
// (alphanumeric) that appears in the error message's path. A short
// needle like "ok" matches by chance — use a dash-bearing token that
// can never be a substring of `/tmp/phptv_bad_<alnum>`.
$secret = 'do-not-echo-this-value';
file_put_contents($bad, "[auth]\ntoken = \"$secret\"\ngarbage no equals sign\n");
Config::forgetCache();
$err = assert_throws(
    TomlParseError::class,
    fn() => Config::load($bad),
    'unrecognized line shape throws'
);
assert_true(
    $err !== null && str_contains($err->getMessage(), ':3'),
    'error message contains line number 3'
);
assert_not_contains($err?->getMessage() ?? '', $secret, 'value is NOT echoed in exception');
@unlink($bad);

// Assignment before section
$bad2 = tempnam(sys_get_temp_dir(), 'phptv_bad_');
file_put_contents($bad2, "key = \"value\"\n");
Config::forgetCache();
$err2 = assert_throws(
    TomlParseError::class,
    fn() => Config::load($bad2),
    'assignment before section throws'
);
assert_true(
    $err2 !== null && str_contains($err2->getMessage(), ':1'),
    'error references line 1'
);
@unlink($bad2);

// Malformed section header
$bad3 = tempnam(sys_get_temp_dir(), 'phptv_bad_');
file_put_contents($bad3, "[unter\n");
Config::forgetCache();
assert_throws(
    TomlParseError::class,
    fn() => Config::load($bad3),
    'malformed section throws'
);
@unlink($bad3);

// Trailing comment is preserved
$ok = tempnam(sys_get_temp_dir(), 'phptv_ok_');
file_put_contents($ok, "[auth]\ntoken = \"abc\" # this is a comment\n");
Config::forgetCache();
$c = Config::load($ok);
assert_eq('abc', $c->getString('auth', 'token'), 'trailing comment stripped after string');
@unlink($ok);

// `#` inside a string is preserved
$ok2 = tempnam(sys_get_temp_dir(), 'phptv_ok_');
file_put_contents($ok2, "[auth]\ntoken = \"a#b#c\"\n");
Config::forgetCache();
$c2 = Config::load($ok2);
assert_eq('a#b#c', $c2->getString('auth', 'token'), '# inside string is literal');
@unlink($ok2);

// Missing key throws MissingConfigKey
Config::forgetCache();
$conf = Config::load(make_config());
assert_throws(
    MissingConfigKey::class,
    fn() => $conf->getString('auth', 'no_such_key'),
    'missing key throws'
);

report_done();
