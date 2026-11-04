import { hydrate } from 'svelte';
import '../styles/index.css';
import App from './App.svelte';

// The page arrives prerendered; this attaches to it.
hydrate(App, { target: document.getElementById('app')! });
