<?php

declare(strict_types=1);

/**
 * Minimal TOML loader for the subset of collector.toml the PHP API reads.
 *
 * Grammar accepted:
 *
 *   blank-line     :=  /^\s*$/
 *   comment        :=  /^\s*#.*$/
 *   section-header :=  /^\s*\[ <name> \]\s*$/
 *   assignment     :=  /^\s*<key>\s*=\s*<value>\s*(#.*)?$/
 *   <name>, <key>  :=  /[A-Za-z0-9_-]+/
 *   <value>        :=  "<string>" | <int> | true | false
 *
 * Anything else throws TomlParseError with the offending line number.
 * We deliberately do NOT support: multi-line strings, arrays, inline
 * tables, dotted keys, dates. The collector.toml documented in §7.3
 * doesn't need them and silent-ignoring an unrecognized shape would
 * mask config drift.
 */

class TomlParseError extends \RuntimeException
{
}

class MissingConfigKey extends \RuntimeException
{
}

final class Config
{
    /** @var array<string, array<string, scalar>> */
    private array $sections;
    private string $path;

    /** @var Config|null process-local cache for the default path */
    private static ?Config $cachedDefault = null;
    private static ?string $cachedDefaultPath = null;

    public const DEFAULT_PATH = '/etc/php-tree-viz/collector.toml';

    /**
     * @param array<string, array<string, scalar>> $sections
     */
    private function __construct(array $sections, string $path)
    {
        $this->sections = $sections;
        $this->path = $path;
    }

    /**
     * Load and parse the config file.
     *
     * - With an explicit $path: load it and cache as "the" config.
     *   Replaces any prior cached instance.
     * - With no $path: return the cached instance if one exists;
     *   otherwise load from $PHPTV_CONFIG or the documented default.
     *
     * `$force=true` always re-reads the resolved path.
     *
     * The "no arg returns cached" rule matches how production wants
     * to behave (load-once) and how tests want to behave (override the
     * path once, then all helpers consult the cache).
     */
    public static function load(?string $path = null, bool $force = false): self
    {
        if ($path === null && !$force && self::$cachedDefault !== null) {
            return self::$cachedDefault;
        }
        $resolvedPath = $path ?? (getenv('PHPTV_CONFIG') ?: self::DEFAULT_PATH);
        if (
            !$force
            && self::$cachedDefault !== null
            && self::$cachedDefaultPath === $resolvedPath
        ) {
            return self::$cachedDefault;
        }

        if (!is_readable($resolvedPath)) {
            throw new \RuntimeException(
                "config file is not readable: {$resolvedPath}"
            );
        }
        $raw = file_get_contents($resolvedPath);
        if ($raw === false) {
            throw new \RuntimeException(
                "config file read failed: {$resolvedPath}"
            );
        }
        $sections = self::parse($raw, $resolvedPath);

        self::$cachedDefault = new self($sections, $resolvedPath);
        self::$cachedDefaultPath = $resolvedPath;
        return self::$cachedDefault;
    }

    /**
     * Forget the cached default. Used by tests that need to load a
     * different fixture path after the first call.
     */
    public static function forgetCache(): void
    {
        self::$cachedDefault = null;
        self::$cachedDefaultPath = null;
    }

    /**
     * Return whatever Config instance is currently cached, or null if
     * none has been loaded. Used by code paths that need "the config
     * already in use" without specifying a path (notably the error
     * logger, which can be invoked from anywhere).
     */
    public static function cached(): ?self
    {
        return self::$cachedDefault;
    }

    public function path(): string
    {
        return $this->path;
    }

    public function getString(string $section, string $key): string
    {
        $v = $this->raw($section, $key);
        if (!is_string($v)) {
            throw new MissingConfigKey(
                "config {$section}.{$key} is not a string"
            );
        }
        return $v;
    }

    public function getInt(string $section, string $key): int
    {
        $v = $this->raw($section, $key);
        if (!is_int($v)) {
            throw new MissingConfigKey(
                "config {$section}.{$key} is not an integer"
            );
        }
        return $v;
    }

    public function getBool(string $section, string $key, bool $default = false): bool
    {
        if (!isset($this->sections[$section][$key])) {
            return $default;
        }
        $v = $this->sections[$section][$key];
        if (!is_bool($v)) {
            throw new MissingConfigKey(
                "config {$section}.{$key} is not a boolean"
            );
        }
        return $v;
    }

    /**
     * Return the raw token/cookie sentinels the logger should redact.
     * Pulled here because the response module needs them before it can
     * load a Config of its own.
     *
     * @return list<string>
     */
    public function logRedactionSentinels(): array
    {
        $sentinels = [];
        if (isset($this->sections['auth']['token']) && is_string($this->sections['auth']['token'])) {
            $sentinels[] = $this->sections['auth']['token'];
        }
        if (isset($this->sections['auth']['session_salt']) && is_string($this->sections['auth']['session_salt'])) {
            $sentinels[] = $this->sections['auth']['session_salt'];
        }
        return $sentinels;
    }

    private function raw(string $section, string $key): bool|int|string
    {
        if (!isset($this->sections[$section][$key])) {
            throw new MissingConfigKey(
                "config key {$section}.{$key} not found"
            );
        }
        return $this->sections[$section][$key];
    }

    /**
     * @return array<string, array<string, scalar>>
     */
    private static function parse(string $raw, string $path): array
    {
        $lines = preg_split('/\r\n|\n|\r/', $raw);
        if ($lines === false) {
            throw new TomlParseError("could not split config file: {$path}");
        }

        $sections = [];
        $current = null;

        foreach ($lines as $i => $line) {
            $lineNum = $i + 1;
            $trimmed = trim($line);

            if ($trimmed === '' || $trimmed[0] === '#') {
                continue;
            }

            if ($trimmed[0] === '[') {
                if (!preg_match('/^\[([A-Za-z0-9_-]+)\]$/', $trimmed, $m)) {
                    throw new TomlParseError(
                        "config syntax error at {$path}:{$lineNum}: malformed section header"
                    );
                }
                $current = $m[1];
                if (!isset($sections[$current])) {
                    $sections[$current] = [];
                }
                continue;
            }

            if ($current === null) {
                throw new TomlParseError(
                    "config syntax error at {$path}:{$lineNum}: assignment before any section"
                );
            }

            if (!preg_match('/^([A-Za-z0-9_-]+)\s*=\s*(.*)$/', $trimmed, $m)) {
                throw new TomlParseError(
                    "config syntax error at {$path}:{$lineNum}: not a section, comment, or key=value line"
                );
            }
            $key = $m[1];
            $rest = trim(self::stripTrailingComment($m[2]));

            $sections[$current][$key] = self::parseScalar($rest, $path, $lineNum);
        }

        return $sections;
    }

    /**
     * Strip a trailing `# comment` from a value. Respects `#` inside
     * a double-quoted string.
     */
    private static function stripTrailingComment(string $value): string
    {
        $inString = false;
        $escape = false;
        $len = strlen($value);
        for ($i = 0; $i < $len; $i++) {
            $ch = $value[$i];
            if ($inString) {
                if ($escape) {
                    $escape = false;
                    continue;
                }
                if ($ch === '\\') {
                    $escape = true;
                    continue;
                }
                if ($ch === '"') {
                    $inString = false;
                }
                continue;
            }
            if ($ch === '"') {
                $inString = true;
                continue;
            }
            if ($ch === '#') {
                return substr($value, 0, $i);
            }
        }
        return $value;
    }

    private static function parseScalar(string $rest, string $path, int $lineNum): bool|int|string
    {
        if ($rest === '') {
            throw new TomlParseError(
                "config syntax error at {$path}:{$lineNum}: empty value"
            );
        }

        if ($rest === 'true') {
            return true;
        }
        if ($rest === 'false') {
            return false;
        }

        if ($rest[0] === '"') {
            if (!preg_match('/^"((?:\\\\.|[^"\\\\])*)"$/', $rest, $m)) {
                throw new TomlParseError(
                    "config syntax error at {$path}:{$lineNum}: unterminated or malformed string"
                );
            }
            return self::unescapeString($m[1]);
        }

        if (preg_match('/^-?[0-9_]+$/', $rest)) {
            $compact = str_replace('_', '', $rest);
            if (!preg_match('/^-?[0-9]+$/', $compact)) {
                throw new TomlParseError(
                    "config syntax error at {$path}:{$lineNum}: malformed integer"
                );
            }
            return (int) $compact;
        }

        throw new TomlParseError(
            "config syntax error at {$path}:{$lineNum}: value is not a string, integer, or boolean"
        );
    }

    private static function unescapeString(string $inner): string
    {
        return preg_replace_callback(
            '/\\\\(.)/',
            static function (array $m): string {
                return match ($m[1]) {
                    '"'  => '"',
                    '\\' => '\\',
                    'n'  => "\n",
                    'r'  => "\r",
                    't'  => "\t",
                    default => throw new TomlParseError(
                        "unknown escape sequence \\{$m[1]}"
                    ),
                };
            },
            $inner
        ) ?? $inner;
    }
}
