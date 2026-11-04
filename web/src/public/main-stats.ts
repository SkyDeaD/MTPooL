import { hydrate } from 'svelte';
import '../styles/index.css';
import Stats from './Stats.svelte';

// The page arrives prerendered; this attaches to it.
hydrate(Stats, { target: document.getElementById('app')! });
