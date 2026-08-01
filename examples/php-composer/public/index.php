<?php

declare(strict_types=1);

use App\Greeting;
use Psr\Http\Message\ResponseInterface;
use Psr\Http\Message\ServerRequestInterface;
use Slim\Factory\AppFactory;

require __DIR__ . '/../vendor/autoload.php';

$app = AppFactory::create();

$app->get('/', function (ServerRequestInterface $request, ResponseInterface $response): ResponseInterface {
    $response->getBody()->write(Greeting::text());

    return $response->withHeader('Content-Type', 'text/plain');
});

$app->run();
