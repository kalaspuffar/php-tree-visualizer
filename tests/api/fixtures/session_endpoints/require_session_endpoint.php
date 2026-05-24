<?php

declare(strict_types=1);

require_once __DIR__ . '/../../../../api/internal/config.php';
require_once __DIR__ . '/../../../../api/internal/response.php';
require_once __DIR__ . '/../../../../api/internal/session.php';

require_session();
phptv_emit_status(204);
phptv_emit_header('Content-Type: application/json');
exit;
