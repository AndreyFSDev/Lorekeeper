import { mount } from 'svelte';
import App from './App.svelte';
import './app.css';

const splash = document.getElementById('splash');
if (splash) splash.remove();

mount(App, { target: document.getElementById('app')! });
