//TODO: crear sistema de ajustes y optener los lenguajes por su api
/*DOC: donde se permita configurar el idioma es un select con un option que lo que hace es tener el ISO del idioma establecido ahí, 
para evitar tener un json con los idiomas y su ISO*/


let main_laguage;//the primary language chosen by the user
let secondary_language;//the fallback language chosen by the user
let ternary_language;//the app's default fallback language

let translation;
let translation_fallback;
let translation_fallback_app;

async function get_translated_text(text=undefined){
  if(!main_laguage) await get_languages()

  if(!translation){
    await fetch(get_translations_Path(main_laguage)).then(res=>res.ok?translation=res.json():()=>{console.warn(`The ${main_laguage} translation could not be loaded.`) ;return null})
  }

  if(!translation[text]){
    if(!translation_fallback){
      await fetch(get_translations_Path(secondary_language)).then(res=>res.ok?translation_fallback=res.json():()=>{console.warn(`The ${secondary_language} translation could not be loaded.`) ;return null})
    }
    if(translation_fallback[text]) return translation_fallback[text]
    else{
      await fetch(get_translations_Path(ternary_language)).then(res=>res.ok?translation_fallback_app=res.json():()=>{console.warn(`The ${ternary_language} translation could not be loaded.`) ;return null})
      
      if(translation_fallback_app[text]) return translation_fallback_app[text]
    }

    return ''
  }
  else return translation[text]
}

async function get_languages(){
  //TODO: llamada a la api de los ajustes para obtener los lenguajes y actualizar las variables
}

const get_translations_Path=(language)=>new URL(`${language}.json`,import.meta.url);