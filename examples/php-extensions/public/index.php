<?php

declare(strict_types=1);

// Extensions are compiled into the PHP installation, not into /app. If the
// runtime image did not receive them this returns 500, even though the build
// succeeded — the exact failure a build-only check cannot see.
$required = ['intl', 'pdo_pgsql'];
$missing = array_values(array_filter(
    $required,
    static fn (string $ext): bool => !extension_loaded($ext)
));

header('Content-Type: text/plain');

if ($missing !== []) {
    http_response_code(500);
    echo 'missing extensions: ' . implode(', ', $missing) . "\n";
    exit;
}

// Actually call into ICU, so a stub would not pass either.
$formatter = new NumberFormatter('en_US', NumberFormatter::DECIMAL);
echo "hello from autopack (intl: {$formatter->format(1234.5)}, pdo_pgsql loaded)\n";
