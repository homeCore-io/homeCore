// The template's UI needs no bundler: its one module is written as it ships.
// "Building" copies it and the manifest into dist/, where the plugin serves
// them from (and which the release packs as ui/). Swap this for Vite (see
// plugins/hc-ecowitt/ui) once the screens want a framework.
import { cpSync, mkdirSync, rmSync } from 'node:fs';

rmSync('dist', { recursive: true, force: true });
mkdirSync('dist');
cpSync('src', 'dist', { recursive: true });
cpSync('public', 'dist', { recursive: true });
console.log('built dist/');
