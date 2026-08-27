//TODO: crear sistema de ajustes y optener los lenguajes por su api
/*DOC: donde se permita configurar el idioma es un select con un option que lo que hace es tener el ISO del idioma establecido ahí, 
para evitar tener un json con los idiomas y su ISO*/
/*DOC: los json de cada traduccion seran pequeños, no hace falta meter metodos de limpieza de cache; 
lo que se hace es no cargar traducciones innecesarias. Ni tampoco optimizaciones de carga ni relacionados */

let main_laguage; //the primary language chosen by the user
let secondary_language; //the fallback language chosen by the user
let ternary_language; //the app's default fallback language

let translation;
let translation_fallback;
let translation_fallback_app;

export async function get_translated_text(text = undefined) {
  if (!main_laguage) await get_languages();

  if (!translation) {
    try {
      translation = await Bun.file(get_translations_Path(main_laguage)).json();
    } catch {
      console.warn(`The ${main_laguage} translation could not be loaded.`);
      translation = null;
    }
  }

  if (translation[text]) return translation[text];

  if (!translation_fallback) {
    try {
      translation_fallback = await Bun.file(
        get_translations_Path(secondary_language),
      ).json();
    } catch {
      console.warn(
        `The ${secondary_language} translation could not be loaded.`,
      );
      translation_fallback = null;
    }
  }
  if (translation_fallback[text]) return translation_fallback[text];
  
  try {
    translation_fallback_app = await Bun.file(
      get_translations_Path(ternary_language),
    ).json();
  } catch {
    console.warn(`The ${ternary_language} translation could not be loaded.`);
    translation_fallback_app = null;
  }
  if (translation_fallback_app[text]) return translation_fallback_app[text];

  return "";
}

async function get_languages() {
  //TODO: llamada a la api de los ajustes para obtener los lenguajes y actualizar las variables
}

const get_translations_Path = (language) =>
  new URL(`${language}.json`, import.meta.url);

export function clear_caches_translations_languages() {
  translation = null;
  translation_fallback = null;
  translation_fallback_app = null;
}
