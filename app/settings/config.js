import { appConfigDir } from "@tauri-apps/api/path";

const CONFIGROOTPATH = await appConfigDir();
const SUBPATHS = {
  app: "appSettings.json",
};

/*Main function to obtain the settings
 *
 * @parameters:
 * "config" must have a SUBPATH key pointing to the desired path
 * "settings" must have the keys for the settings to be obtained from the configuration file
 *
 * @return:
 * Although "settings" is an array, the return will be an object that uses
 * the array values ​​as keys and the value of each key as the value of that setting.
 */

async function getSetting(settings = [], config = "app") {
  const ERROR = () => {
    const obj = Object.fromEntries(settings.map((key) => [key, undefined]));
    return { succes: false, value: obj };
  };

  if (!(settings in SUBPATHS)) {
    ERROR();
  }
  try {
    const res = await Bun.file(
      new URL(SUBPATHS[config], import.meta.url),
    ).json();

    const obj = Object.fromEntries(
      settings.map((key) => {
        const value = key in res ? res[key] : undefined;
        return [key, value];
      }),
    );
    return { succes: false, value: obj };
  } catch {
    ERROR();
  }
}
