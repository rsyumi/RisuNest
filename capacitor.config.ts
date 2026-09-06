import { CapacitorConfig } from '@capacitor/cli';

const config: CapacitorConfig = {
  appId: 'io.github.rsyumi.risunest',
  appName: 'RisuNest',
  webDir: 'dist',
  server: {
    androidScheme: 'https'
  }
};

export default config;
