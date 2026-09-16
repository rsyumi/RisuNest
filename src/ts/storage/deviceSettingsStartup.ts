import { isTauri } from '../platform'
import { getDeviceSettings } from './deviceSettings'

// A native install reads the device file, so its settings only become available
// once the start opens that file.
if (!isTauri) getDeviceSettings()
