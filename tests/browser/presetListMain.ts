import '../../src/styles.css'
import { mount } from 'svelte'
import PresetListHost from './PresetListHost.svelte'

mount(PresetListHost, { target: document.querySelector('#host')! })
