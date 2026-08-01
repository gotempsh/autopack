<?php

declare(strict_types=1);

namespace App;

/** Exists so the example actually exercises the PSR-4 autoloader. */
final class Greeting
{
    public static function text(): string
    {
        return "hello from autopack\n";
    }
}
