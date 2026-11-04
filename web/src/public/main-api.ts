import { hydrate } from 'svelte';
import '../styles/index.css';
import ApiDocs from './ApiDocs.svelte';

hydrate(ApiDocs, { target: document.getElementById('app')! });
