import { appConfigDir } from "@tauri-apps/api/path";
import { clear_caches_translations_languages } from "../translations/language.js";
const CONFIGROOTPATH = await appConfigDir();
const SUBPATHS = {
  app: "appSettings.json",
};

/*Generic main function to obtain the settings
 *
 * @parameters:
 * "config" must have a SUBPATH key pointing to the desired path
 * "settings" must have the keys for the settings to be obtained from the configuration file
 *
 * @return:
 * Although "settings" is an array, the return will be an object that uses
 * the array values ​​as keys and the value of each key as the value of that setting.
 */

export async function getSetting({ settings = [], config = "app" }) {
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

/*
 * Generic main function to assign a value to a setting
 * Maintains the same style as getSetting
 *
 * @parameters:
 * "settings" is an object where the key is the setting and its value is the value you want to assign to the setting
 */
export async function setSetting({ settings = {}, config = "app" }) {
  const settingPath = new URL(SUBPATHS[config], import.meta.url);
  let res = await Bun.file(settingPath)
    .json()
    .catch(() => ({}));
  const newSettingss = { ...res, ...settings };
  await Bun.write(settingPath, JSON.stringify(newSettingss, null, 2));
}

export async function changeLanguage({
  newLanguage = "en",
  setting = "mainLanguage",
}) {
  await setSetting({ settings: { setting: newLanguage }, config: "app" });
  clear_caches_translations_languages();
}

export async function defaultSettings() {
  const settingsApp = {
    mainLanguage: "en",
    secondary_language: "en",
    ternary_language: "en",
  };

  await setSetting({ settings: settingsApp, config: "app" });
}
