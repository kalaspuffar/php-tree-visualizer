<?php

declare(strict_types=1);

require_once __DIR__ . '/../../../../api/internal/config.php';
require_once __DIR__ . '/../../../../api/internal/response.php';

json_success(200, ['hello' => 'world', 'count' => 3]);
